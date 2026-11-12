//! `pciscan`: a standalone UEFI app that enumerates the PCI bus and dumps each
//! device's configuration header, like `lspci`. It proves we can touch Wi-Fi
//! silicon and read registers back from bare metal with no OS underneath.
//!
//! It flags any Intel wireless NIC (vendor 0x8086, network class) and calls out
//! the ThinkPad X1 Gen 6's Intel 8265 (device 0x24fd) specifically. Under QEMU
//! you'll see the emulated chipset (host bridge, SATA, any `-device` NICs); on
//! real hardware, or via VFIO passthrough of a real card, the 8265 shows up
//! with its true BARs.
//!
//! Build/run it on its own:  `just pciscan`  (graphical)  /  `just pciscan-serial`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use core::fmt::Write;

use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams, ScopedProtocol};
use uefi::prelude::*;
use uefi::proto::pci::root_bridge::PciRootBridgeIo;

/// Intel's PCI vendor ID.
const INTEL: u16 = 0x8086;
/// PCI base class 0x02 = network controller.
const CLASS_NETWORK: u8 = 0x02;
/// The X1 Carbon Gen 6's Intel Dual Band Wireless-AC 8265.
const DEV_8265: u16 = 0x24fd;

/// MMIO offsets into the Intel WiFi CSR block (from iwlwifi's `iwl-csr.h`).
/// Reading these proves we're talking to the chip's registers, not just its
/// PCI config header.
const CSR_HW_IF_CONFIG: u64 = 0x000;
const CSR_HW_REV: u64 = 0x028;

/// A couple of live CSR values read over MMIO from an Intel WiFi chip.
struct WifiRegs {
    hw_if_config: u32,
    hw_rev: u32,
}

/// A flattened snapshot of one PCI function's config header.
struct Dev {
    seg: u32,
    bus: u8,
    dev: u8,
    fun: u8,
    vendor: u16,
    device: u16,
    revision: u8,
    class: u8,
    subclass: u8,
    prog_if: u8,
    subsys_vendor: u16,
    subsys_device: u16,
    command: u16,
    bar0: u64,
    bar0_is_io: bool,
    bar0_is_64: bool,
    wifi_regs: Option<WifiRegs>,
}

#[entry]
fn main() -> Status {
    uefi::helpers::init().unwrap();

    let devices = scan();

    system::with_stdout(|out| {
        let _ = out.clear();
        let _ = writeln!(out, "ferrite-uefi :: pciscan  -  touching the chip\r");
        let _ = writeln!(out, "seg  bus:dev.fn  vend:dev   rev  class     subsys      cmd   bar0\r");
        let _ = writeln!(out, "-------------------------------------------------------------------------\r");

        let mut wifi_found = false;
        for d in &devices {
            print_dev(out, d);
            if d.vendor == INTEL && (d.subclass == 0x80 || d.device == DEV_8265) {
                wifi_found = true;
            }
        }

        let _ = writeln!(out, "-------------------------------------------------------------------------\r");
        let _ = writeln!(out, "{} PCI function(s) enumerated\r", devices.len());
        if wifi_found {
            let _ = writeln!(out, "*** Intel wireless NIC present - we can read its registers ***\r");
        } else {
            let _ = writeln!(out, "(no Intel wireless NIC here - expected under stock QEMU; use real HW or VFIO)\r");
        }
        let _ = writeln!(out, "\r\npress a key to exit...\r");
    });

    wait_for_key();
    Status::SUCCESS
}

/// Enumerate every PCI root bridge in the system and flatten all discovered
/// functions into a single list.
fn scan() -> Vec<Dev> {
    let mut out = Vec::new();
    let Ok(handles) = boot::find_handles::<PciRootBridgeIo>() else {
        return out;
    };
    for handle in handles {
        // Non-exclusive GetProtocol: firmware's own PCI bus driver already holds
        // the root bridge open, so an exclusive open would be denied.
        let params = OpenProtocolParams { handle, agent: boot::image_handle(), controller: None };
        let Ok(mut rb) =
            (unsafe { boot::open_protocol::<PciRootBridgeIo>(params, OpenProtocolAttributes::GetProtocol) })
        else {
            continue;
        };
        let seg = rb.segment_nr();
        let Ok(tree) = rb.enumerate() else { continue };
        // Collect addresses first so the immutable tree borrow ends before we
        // take the `&mut rb` needed for config-space reads.
        let addrs: Vec<_> = tree.iter().copied().collect();
        for addr in addrs {
            if let Some(dev) = read_dev(&mut rb, seg, addr) {
                out.push(dev);
            }
        }
    }
    out
}

/// Read a 32-bit dword from a device's PCI configuration space at byte `off`.
fn cfg_rd(rb: &mut ScopedProtocol<PciRootBridgeIo>, addr: uefi::proto::pci::PciIoAddress, off: u8) -> Option<u32> {
    rb.pci().read_one::<u32>(addr.with_register(off)).ok()
}

/// Read one function's config header. Returns `None` for absent functions.
fn read_dev(rb: &mut ScopedProtocol<PciRootBridgeIo>, seg: u32, addr: uefi::proto::pci::PciIoAddress) -> Option<Dev> {
    let reg0 = cfg_rd(rb, addr, 0x00)?;
    let vendor = (reg0 & 0xffff) as u16;
    if vendor == 0xffff {
        return None; // no function here
    }
    let device = (reg0 >> 16) as u16;

    let reg_cmd = cfg_rd(rb, addr, 0x04).unwrap_or(0);
    let reg_class = cfg_rd(rb, addr, 0x08).unwrap_or(0);
    let reg_sub = cfg_rd(rb, addr, 0x2c).unwrap_or(0);
    let bar0_lo = cfg_rd(rb, addr, 0x10).unwrap_or(0);

    let bar0_is_io = bar0_lo & 0x1 != 0;
    let bar0_is_64 = !bar0_is_io && (bar0_lo & 0x6) == 0x4;
    let bar0 = if bar0_is_io {
        u64::from(bar0_lo & 0xffff_fffc)
    } else if bar0_is_64 {
        let hi = cfg_rd(rb, addr, 0x14).unwrap_or(0);
        (u64::from(hi) << 32) | u64::from(bar0_lo & 0xffff_fff0)
    } else {
        u64::from(bar0_lo & 0xffff_fff0)
    };

    // For an Intel wireless NIC, go one level deeper than the config header:
    // read live registers out of its MMIO CSR block. This is the real proof we
    // can touch the chip. Ensure Memory Space decode is enabled first (bit 1 of
    // the command register); firmware usually has it set, but be sure.
    let class = ((reg_class >> 24) & 0xff) as u8;
    let wifi_regs = if vendor == INTEL && class == CLASS_NETWORK && !bar0_is_io && bar0 != 0 {
        let cmd = (reg_cmd & 0xffff) as u16;
        if cmd & 0x2 == 0 {
            let _ = rb.pci().write_one::<u32>(addr.with_register(0x04), reg_cmd | 0x2);
        }
        let hw_if_config = rb.memory().read_one::<u32>(bar0 + CSR_HW_IF_CONFIG).ok();
        let hw_rev = rb.memory().read_one::<u32>(bar0 + CSR_HW_REV).ok();
        match (hw_if_config, hw_rev) {
            (Some(hw_if_config), Some(hw_rev)) => Some(WifiRegs { hw_if_config, hw_rev }),
            _ => None,
        }
    } else {
        None
    };

    Some(Dev {
        seg,
        bus: addr.bus,
        dev: addr.dev,
        fun: addr.fun,
        vendor,
        device,
        revision: (reg_class & 0xff) as u8,
        prog_if: ((reg_class >> 8) & 0xff) as u8,
        subclass: ((reg_class >> 16) & 0xff) as u8,
        class,
        subsys_vendor: (reg_sub & 0xffff) as u16,
        subsys_device: (reg_sub >> 16) as u16,
        command: (reg_cmd & 0xffff) as u16,
        bar0,
        bar0_is_io,
        bar0_is_64,
        wifi_regs,
    })
}

fn print_dev(out: &mut uefi::proto::console::text::Output, d: &Dev) {
    let bar_kind = if d.bar0_is_io {
        "io"
    } else if d.bar0_is_64 {
        "m64"
    } else {
        "m32"
    };
    let _ = write!(
        out,
        "{:04x} {:02x}:{:02x}.{}  {:04x}:{:04x}  {:02x}   {:02x}:{:02x}:{:02x}  {:04x}:{:04x}  {:04x}  {}:{:08x}",
        d.seg,
        d.bus,
        d.dev,
        d.fun,
        d.vendor,
        d.device,
        d.revision,
        d.class,
        d.subclass,
        d.prog_if,
        d.subsys_vendor,
        d.subsys_device,
        d.command,
        bar_kind,
        d.bar0,
    );
    let _ = write!(out, "  {}", class_name(d.class, d.subclass));
    if d.vendor == INTEL && d.class == CLASS_NETWORK {
        if d.device == DEV_8265 {
            let _ = write!(out, "  <== Intel 8265 (X1 Gen 6 target!)");
        } else if d.subclass == 0x80 {
            let _ = write!(out, "  <== Intel wireless");
        } else {
            let _ = write!(out, "  <== Intel NIC");
        }
    }
    let _ = writeln!(out, "\r");

    if let Some(regs) = &d.wifi_regs {
        let _ = writeln!(
            out,
            "           `- MMIO CSR read: HW_REV={:#010x}  HW_IF_CONFIG={:#010x}\r",
            regs.hw_rev, regs.hw_if_config
        );
    }
}

/// A terse human label for the PCI base/sub class.
const fn class_name(class: u8, subclass: u8) -> &'static str {
    match (class, subclass) {
        (0x01, _) => "storage",
        (0x02, 0x80) => "network:other",
        (0x02, _) => "network",
        (0x03, _) => "display",
        (0x04, _) => "multimedia",
        (0x06, 0x00) => "host bridge",
        (0x06, 0x01) => "isa bridge",
        (0x06, 0x04) => "pci bridge",
        (0x06, _) => "bridge",
        (0x0c, 0x03) => "usb",
        (0x0c, _) => "serial bus",
        _ => "",
    }
}

/// Block until the user presses a key so the report stays on screen.
fn wait_for_key() {
    system::with_stdin(|input| {
        if let Ok(event) = input.wait_for_key_event() {
            let _ = boot::wait_for_event(&mut [event]);
        }
    });
}
