//! Find every xHCI on every PCI root bridge and the bits of it DbC needs.

use alloc::vec::Vec;
use core::fmt;

use uefi::Handle;
use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams, ScopedProtocol};
use uefi::proto::pci::PciIoAddress;
use uefi::proto::pci::root_bridge::PciRootBridgeIo;

use super::regs::{XCAP_DEBUG, XCAP_LEGACY, XCAP_SUPPORTED_PROTOCOL};

const XHCI_CLASS: u32 = 0x0c_0330;
const PCI_COMMAND: u8 = 0x04;
const CMD_MEMORY: u16 = 1 << 1;
const CMD_BUS_MASTER: u16 = 1 << 2;

/// Config and MMIO access through one root bridge. MMIO reads that fail come
/// back as all ones, like a master-aborted read on real hardware.
pub struct RootBridge(ScopedProtocol<PciRootBridgeIo>);

impl RootBridge {
    #[must_use]
    pub fn open(handle: Handle) -> Option<Self> {
        let params = OpenProtocolParams {
            handle,
            agent: boot::image_handle(),
            controller: None,
        };
        // SAFETY: GetProtocol does not take ownership; firmware drivers keep their own use.
        unsafe {
            boot::open_protocol::<PciRootBridgeIo>(params, OpenProtocolAttributes::GetProtocol)
        }
        .ok()
        .map(Self)
    }

    #[must_use]
    pub fn segment(&self) -> u32 {
        self.0.segment_nr()
    }

    pub fn enumerate(&mut self) -> Vec<PciIoAddress> {
        self.0
            .enumerate()
            .map_or_default(|tree| tree.iter().copied().collect())
    }

    pub fn cfg_read32(&mut self, addr: PciIoAddress, off: u8) -> Option<u32> {
        self.0.pci().read_one::<u32>(addr.with_register(off)).ok()
    }

    pub fn cfg_read16(&mut self, addr: PciIoAddress, off: u8) -> Option<u16> {
        self.0.pci().read_one::<u16>(addr.with_register(off)).ok()
    }

    pub fn cfg_write16(&mut self, addr: PciIoAddress, off: u8, val: u16) -> bool {
        self.0
            .pci()
            .write_one::<u16>(addr.with_register(off), val)
            .is_ok()
    }

    pub fn read32(&mut self, phys: u64) -> u32 {
        self.0.memory().read_one::<u32>(phys).unwrap_or(u32::MAX)
    }

    pub fn write32(&mut self, phys: u64, val: u32) {
        let _ = self.0.memory().write_one::<u32>(phys, val);
    }

    pub fn read64(&mut self, phys: u64) -> u64 {
        let lo = u64::from(self.read32(phys));
        lo | (u64::from(self.read32(phys + 4)) << 32)
    }

    /// Low dword first, as the spec requires for registers with side effects on the high half.
    pub fn write64(&mut self, phys: u64, val: u64) {
        self.write32(phys, val as u32);
        self.write32(phys + 4, (val >> 32) as u32);
    }

    /// PCI power state (0 = D0) and the PMCSR config offset, if the device has a PM capability.
    pub fn power_state(&mut self, addr: PciIoAddress) -> Option<(u8, u8)> {
        let status = self.cfg_read32(addr, PCI_COMMAND)? >> 16;
        if status & 0x10 == 0 {
            return None;
        }
        let mut ptr = (self.cfg_read32(addr, 0x34)? & 0xfc) as u8;
        for _ in 0..48 {
            if ptr == 0 {
                break;
            }
            let cap = self.cfg_read32(addr, ptr)?;
            if cap & 0xff == 0x01 {
                let pmcsr_off = ptr + 4;
                let pmcsr = self.cfg_read32(addr, pmcsr_off)?;
                return Some((pmcsr_off, (pmcsr & 0x3) as u8));
            }
            ptr = ((cap >> 8) & 0xfc) as u8;
        }
        None
    }

    /// Turn on memory decode and bus mastering; returns the command register as found.
    pub fn enable_dma(&mut self, addr: PciIoAddress) -> Option<u16> {
        let cmd = self.cfg_read16(addr, PCI_COMMAND)?;
        let want = cmd | CMD_MEMORY | CMD_BUS_MASTER;
        if want != cmd {
            self.cfg_write16(addr, PCI_COMMAND, want);
        }
        Some(cmd)
    }
}

/// An xHCI controller as found on a root bridge.
#[derive(Clone, Debug)]
pub struct XhciDevice {
    pub bridge: Handle,
    pub segment: u32,
    pub addr: PciIoAddress,
    pub vendor: u16,
    pub device: u16,
    pub mmio: u64,
    pub cap_length: u8,
    pub max_ports: u8,
    /// Absolute address of the DbC register block, if the controller has one.
    pub dbc: Option<u64>,
    /// Absolute address of the USB legacy support capability, if present.
    pub legacy: Option<u64>,
    /// (first port, count) of every USB 3 port range; ports are 1-based.
    pub usb3_ports: Vec<(u8, u8)>,
}

impl XhciDevice {
    #[must_use]
    pub fn bdf(&self) -> (u8, u8, u8) {
        (self.addr.bus, self.addr.dev, self.addr.fun)
    }

    #[must_use]
    pub fn op_base(&self) -> u64 {
        self.mmio + u64::from(self.cap_length)
    }
}

impl fmt::Display for XhciDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (b, d, n) = self.bdf();
        write!(
            f,
            "{b:02x}:{d:02x}.{n} {:04x}:{:04x}",
            self.vendor, self.device
        )
    }
}

/// Every xHCI across all root bridges, deduplicated by segment and address.
#[must_use]
pub fn find_xhcis() -> Vec<XhciDevice> {
    let mut found: Vec<XhciDevice> = Vec::new();
    let Ok(handles) = boot::find_handles::<PciRootBridgeIo>() else {
        return found;
    };
    for handle in handles {
        let Some(mut rb) = RootBridge::open(handle) else {
            continue;
        };
        let segment = rb.segment();
        for addr in rb.enumerate() {
            let addr = addr.with_register(0);
            let dup = found
                .iter()
                .any(|d| d.segment == segment && u64::from(d.addr) == u64::from(addr));
            if dup {
                continue;
            }
            if let Some(dev) = probe(&mut rb, handle, segment, addr) {
                found.push(dev);
            }
        }
    }
    found
}

fn probe(
    rb: &mut RootBridge,
    bridge: Handle,
    segment: u32,
    addr: PciIoAddress,
) -> Option<XhciDevice> {
    let id = rb.cfg_read32(addr, 0x00)?;
    if id & 0xffff == 0xffff {
        return None;
    }
    if rb.cfg_read32(addr, 0x08)? >> 8 != XHCI_CLASS {
        return None;
    }

    rb.enable_dma(addr)?;
    let bar_lo = rb.cfg_read32(addr, 0x10)?;
    let mut mmio = u64::from(bar_lo & 0xffff_fff0);
    if bar_lo & 1 == 0 && bar_lo & 0x6 == 0x4 {
        mmio |= u64::from(rb.cfg_read32(addr, 0x14)?) << 32;
    }
    if mmio == 0 {
        return None;
    }

    let cap0 = rb.read32(mmio);
    if cap0 == u32::MAX {
        return None;
    }
    let cap_length = (cap0 & 0xff) as u8;
    let max_ports = (rb.read32(mmio + 0x04) >> 24) as u8;
    let xecp = u64::from(rb.read32(mmio + 0x10) >> 16);

    let mut dev = XhciDevice {
        bridge,
        segment,
        addr,
        vendor: (id & 0xffff) as u16,
        device: (id >> 16) as u16,
        mmio,
        cap_length,
        max_ports,
        dbc: None,
        legacy: None,
        usb3_ports: Vec::new(),
    };
    walk_ext_caps(rb, &mut dev, xecp);
    Some(dev)
}

fn walk_ext_caps(rb: &mut RootBridge, dev: &mut XhciDevice, xecp: u64) {
    if xecp == 0 {
        return;
    }
    let mut off = dev.mmio + xecp * 4;
    for _ in 0..64 {
        let cap = rb.read32(off);
        if cap == u32::MAX {
            return;
        }
        match (cap & 0xff) as u8 {
            XCAP_DEBUG => dev.dbc = Some(off),
            XCAP_LEGACY => dev.legacy = Some(off),
            XCAP_SUPPORTED_PROTOCOL if cap >> 24 == 3 => {
                let ports = rb.read32(off + 8);
                dev.usb3_ports
                    .push(((ports & 0xff) as u8, ((ports >> 8) & 0xff) as u8));
            }
            _ => {}
        }
        let next = u64::from((cap >> 8) & 0xff);
        if next == 0 {
            return;
        }
        off += next * 4;
    }
}
