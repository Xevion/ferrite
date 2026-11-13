//! Whether firmware DMA protection is on, which decides how the kernel arena is made visible to the
//! card: at its own address, or through the device's PCI I/O `Map`.
//!
//! Three signals are read and logged: the EDK2 IOMMU protocol (present when the firmware's VT-d driver
//! enforces pre-boot DMA protection), the ACPI DMAR table's flags and units, and each unit's translation
//! and protected-memory status registers. The registers are read through the root bridge's MMIO, which
//! some firmware refuses outside the PCI apertures; a refused read is logged and counts as unknown.

use alloc::vec::Vec;
use core::fmt;

use ferrite_efi::pci::RootBridge;
use ferrite_hal::{Mmio as _, PhysAddr};
use uefi::boot;
use uefi::proto::pci::root_bridge::PciRootBridgeIo;
use uefi::table::cfg::ConfigTableEntry;

/// VT-d global status: translation enabled.
const GSTS: u64 = 0x1c;
const GSTS_TES: u32 = 1 << 31;
/// VT-d protected memory enable: bit 31 enables, bit 0 reports the regions active.
const PMEN: u64 = 0x64;
const PMEN_PRS: u32 = 1;
/// DMAR flag: the platform asks the OS to keep DMA protection on from boot.
const DMA_CTRL_PLATFORM_OPT_IN: u8 = 1 << 2;

/// How the arena reaches the card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaPath {
    /// The bus address is the address: no protection is active.
    Identity,
    /// One PCI I/O `Map` of the arena for the card, as the firmware's IOMMU requires.
    PciIoMap,
}

/// One DMA remapping unit.
#[derive(Clone, Copy, Debug)]
pub struct Unit {
    pub base: PhysAddr,
    pub segment: u16,
    /// Global status and protected-memory registers, when the read was allowed.
    pub gsts: Option<u32>,
    pub pmen: Option<u32>,
}

impl Unit {
    fn protecting(&self) -> Option<bool> {
        match (self.gsts, self.pmen) {
            (None, None) => None,
            (gsts, pmen) => Some(gsts.is_some_and(|g| g & GSTS_TES != 0) || pmen.is_some_and(|p| p & PMEN_PRS != 0)),
        }
    }
}

impl fmt::Display for Unit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unit at {} (segment {}): ", self.base, self.segment)?;
        match self.gsts {
            Some(g) => write!(f, "GSTS {g:#010x} translation {}", if g & GSTS_TES != 0 { "on" } else { "off" })?,
            None => write!(f, "GSTS unreadable")?,
        }
        match self.pmen {
            Some(p) => write!(f, ", PMEN {p:#010x} protected memory {}", if p & PMEN_PRS != 0 { "on" } else { "off" }),
            None => write!(f, ", PMEN unreadable"),
        }
    }
}

/// What the probe found.
#[derive(Debug)]
pub struct Probe {
    pub iommu_protocol: Option<usize>,
    /// DMAR flags, when the table exists.
    pub dmar_flags: Option<u8>,
    pub units: Vec<Unit>,
}

impl Probe {
    /// Map through PCI I/O when the firmware's IOMMU protocol is there or any unit is translating or
    /// protecting memory; identity only when every signal says off. An unanswered protocol lookup
    /// counts as present.
    pub fn decide(&self) -> DmaPath {
        let protocol = self.iommu_protocol.is_none_or(|count| count > 0);
        let unit = self.units.iter().any(|u| u.protecting() == Some(true));
        if protocol || unit { DmaPath::PciIoMap } else { DmaPath::Identity }
    }
}

/// Read every signal and log it.
pub fn probe() -> Probe {
    let iommu_protocol = match ferrite_efi::dma::iommu_handle_count() {
        Ok(count) => Some(count),
        Err(error) => {
            log::warn!("looking up the EDK2 IOMMU protocol failed: {error}");
            None
        },
    };
    log::info!("EDK2 IOMMU protocol: {iommu_protocol:?} handles");
    let Some((width, flags, mut units)) = dmar() else {
        log::info!("ACPI DMAR: no table");
        return Probe { iommu_protocol, dmar_flags: None, units: Vec::new() };
    };
    log::info!(
        "ACPI DMAR: host address width {}, flags {flags:#04x} (DMA control opt-in {}), {} remapping units",
        u32::from(width) + 1,
        flags & DMA_CTRL_PLATFORM_OPT_IN != 0,
        units.len()
    );
    read_registers(&mut units);
    for unit in &units {
        log::info!("VT-d {unit}");
    }
    Probe { iommu_protocol, dmar_flags: Some(flags), units }
}

fn read_registers(units: &mut [Unit]) {
    if units.is_empty() {
        return;
    }
    let bridge = boot::find_handles::<PciRootBridgeIo>().ok().and_then(|h| h.first().copied()).map(RootBridge::open);
    let mut bridge = match bridge {
        Some(Ok(bridge)) => bridge,
        Some(Err(error)) => {
            log::warn!("opening a root bridge for the VT-d registers failed: {error}");
            return;
        },
        None => {
            log::warn!("no root bridge to read the VT-d registers through");
            return;
        },
    };
    for unit in units {
        let mut read = |offset| match bridge.try_read32(unit.base.offset(offset)) {
            Ok(value) => Some(value),
            Err(error) => {
                log::info!("VT-d register {:#x} at {} unreadable through the root bridge: {error}", offset, unit.base);
                None
            },
        };
        unit.gsts = read(GSTS);
        unit.pmen = read(PMEN);
    }
}

/// Host address width, flags and remapping units of the ACPI DMAR table.
fn dmar() -> Option<(u8, u8, Vec<Unit>)> {
    let rsdp = uefi::system::with_config_table(|tables| {
        tables.iter().find(|t| t.guid == ConfigTableEntry::ACPI2_GUID).map(|t| t.address.cast::<u8>())
    })?;
    // SAFETY: the firmware publishes an ACPI 2.0 RSDP here; ACPI tables are identity mapped firmware
    // memory that lives for the whole boot, and each read stays inside a table's stated length.
    unsafe {
        let xsdt = rsdp.add(24).cast::<u64>().read_unaligned() as usize as *const u8;
        if xsdt.is_null() || xsdt.cast::<[u8; 4]>().read_unaligned() != *b"XSDT" {
            return None;
        }
        let entries = (xsdt.add(4).cast::<u32>().read_unaligned() as usize).saturating_sub(36) / 8;
        let table = (0..entries)
            .map(|i| xsdt.add(36 + i * 8).cast::<u64>().read_unaligned() as usize as *const u8)
            .find(|t| !t.is_null() && t.cast::<[u8; 4]>().read_unaligned() == *b"DMAR")?;
        let length = table.add(4).cast::<u32>().read_unaligned() as usize;
        let (width, flags) = (table.add(36).read(), table.add(37).read());
        let mut units = Vec::new();
        let mut at = 48;
        while at + 4 <= length {
            let kind = table.add(at).cast::<u16>().read_unaligned();
            let size = usize::from(table.add(at + 2).cast::<u16>().read_unaligned());
            if size < 4 {
                break;
            }
            if kind == 0 && size >= 16 {
                let segment = table.add(at + 6).cast::<u16>().read_unaligned();
                let base = PhysAddr::new(table.add(at + 8).cast::<u64>().read_unaligned());
                units.push(Unit { base, segment, gsts: None, pmen: None });
            }
            at += size;
        }
        Some((width, flags, units))
    }
}
