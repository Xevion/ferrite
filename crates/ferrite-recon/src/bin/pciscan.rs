//! `pciscan`: a standalone UEFI app that enumerates the PCI bus and dumps each device's
//! configuration header, like `lspci`. It proves we can touch Wi-Fi silicon and read
//! registers back from bare metal with no OS underneath.
//!
//! It flags any Intel wireless NIC (vendor 0x8086, network class) and calls out the
//! X1 Gen 6's Intel 8265 (device 0x24fd) specifically. Under QEMU you see the
//! emulated chipset; on real hardware the 8265 shows up with its true BARs. Nothing is
//! written to config space: a card whose memory decode is off is reported as such instead
//! of being enabled.
//!
//! The report goes to the console and `\EFI\ferrite\pciscan.txt`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;

use ferrite_efi::bootlog::{BootLog, Echo};
use ferrite_efi::pci::RootBridge;
use ferrite_efi::{logger, session};
use ferrite_hal::{MmioExt, PciAddress, PciConfig, PhysAddr, PlatformError, ReadOnly, Reg};
use ferrite_recon::pci::{Bar, ClassCode, DeviceId, VendorId, split_ids};
use ferrite_xhci::pci::{BAR0, BAR1, CLASS_REV, CMD_MEMORY, COMMAND, ID, bar_is_64bit};
use log::LevelFilter;
use uefi::boot;
use uefi::prelude::*;
use uefi::proto::pci::root_bridge::PciRootBridgeIo;

/// How long to keep the report on screen before exiting.
const EXIT_AFTER_SECS: u64 = 20;

/// Subsystem vendor and device ID.
const SUBSYSTEM: u8 = 0x2c;

/// Intel Wi-Fi CSR registers from the Linux driver's register map: reading them proves
/// we reach the chip itself and not just its config header.
const CSR_HW_IF_CONFIG: Reg<u32, ReadOnly> = Reg::new(0x000);
const CSR_HW_REV: Reg<u32, ReadOnly> = Reg::new(0x028);

/// What reading an Intel wireless card's CSR block produced.
enum Csr {
    Read { hw_rev: u32, hw_if_config: u32 },
    DecodeOff,
    Failed(PlatformError),
}

/// One PCI function's config header.
struct Function {
    addr: PciAddress,
    vendor: VendorId,
    device: DeviceId,
    revision: u8,
    class: ClassCode,
    subsystem: (VendorId, DeviceId),
    command: u16,
    bar0: Bar,
    csr: Option<Csr>,
}

impl Function {
    fn is_intel_network(&self) -> bool {
        self.vendor == VendorId::INTEL && self.class.base == ClassCode::NETWORK
    }

    fn is_intel_wireless(&self) -> bool {
        self.vendor == VendorId::INTEL
            && (self.class.subclass == ClassCode::NETWORK_OTHER || self.device == DeviceId::INTEL_8265)
    }
}

#[entry]
fn main() -> Status {
    let helpers = uefi::helpers::init();
    logger::init(BootLog::open("pciscan", Echo::Console), LevelFilter::Debug);
    if let Err(e) = helpers {
        log::warn!("uefi helpers init failed: {e}");
    }
    report();
    logger::with(|log| session::finish(log, EXIT_AFTER_SECS));
    Status::SUCCESS
}

fn report() {
    log::info!("ferrite-uefi :: pciscan - touching the chip");
    log::info!("address       vend:dev   rev  class     subsys      cmd   bar0");

    let functions = scan();
    for f in &functions {
        print_function(f);
    }

    log::info!("");
    log::info!("{} PCI function(s) enumerated", functions.len());
    if functions.iter().any(Function::is_intel_wireless) {
        log::info!("*** Intel wireless NIC present - we can read its registers ***");
    } else {
        log::info!("(no Intel wireless NIC here - expected under stock QEMU; use real HW or VFIO)");
    }
}

/// Every function behind every root bridge.
fn scan() -> Vec<Function> {
    let handles = match boot::find_handles::<PciRootBridgeIo>() {
        Ok(handles) => handles,
        Err(e) => {
            log::error!("no PCI root bridge protocol available: {e}");
            return Vec::new();
        },
    };
    let mut functions = Vec::new();
    for handle in handles {
        let mut rb = match RootBridge::open(handle) {
            Ok(rb) => rb,
            Err(e) => {
                log::warn!("opening a root bridge failed: {e}");
                continue;
            },
        };
        let addrs = match rb.enumerate() {
            Ok(addrs) => addrs,
            Err(e) => {
                log::warn!("enumerating segment {} failed: {e}", rb.segment());
                continue;
            },
        };
        for addr in addrs {
            match read_function(&mut rb, addr) {
                Ok(Some(f)) => functions.push(f),
                Ok(None) => {},
                Err(e) => log::warn!("{addr}: config read failed: {e}"),
            }
        }
    }
    functions
}

/// Read one function's config header; `Ok(None)` when nothing answers at `addr`.
fn read_function(rb: &mut RootBridge, addr: PciAddress) -> Result<Option<Function>, PlatformError> {
    let (vendor, device) = split_ids(rb.cfg_read32(addr, ID)?);
    if vendor.0 == 0xffff {
        return Ok(None);
    }
    let command = rb.cfg_read16(addr, COMMAND)?;
    let (class, revision) = ClassCode::from_class_rev(rb.cfg_read32(addr, CLASS_REV)?);
    let subsystem = split_ids(rb.cfg_read32(addr, SUBSYSTEM)?);
    let lo = rb.cfg_read32(addr, BAR0)?;
    let hi = if bar_is_64bit(lo) { rb.cfg_read32(addr, BAR1)? } else { 0 };
    let bar0 = Bar::decode(lo, hi);

    let mut function = Function { addr, vendor, device, revision, class, subsystem, command, bar0, csr: None };
    if function.is_intel_network() && bar0.is_mmio() {
        function.csr = Some(read_csr(rb, command, bar0));
    }
    Ok(Some(function))
}

fn read_csr(rb: &mut RootBridge, command: u16, bar0: Bar) -> Csr {
    if command & CMD_MEMORY == 0 {
        return Csr::DecodeOff;
    }
    let base = PhysAddr::new(bar0.base);
    let regs = rb.try_read(base, CSR_HW_REV).and_then(|rev| Ok((rev, rb.try_read(base, CSR_HW_IF_CONFIG)?)));
    match regs {
        Ok((hw_rev, hw_if_config)) => Csr::Read { hw_rev, hw_if_config },
        Err(e) => Csr::Failed(e),
    }
}

fn print_function(f: &Function) {
    let tag = if !f.is_intel_network() {
        ""
    } else if f.device == DeviceId::INTEL_8265 {
        "  <== Intel 8265 (X1 Gen 6 target!)"
    } else if f.class.subclass == ClassCode::NETWORK_OTHER {
        "  <== Intel wireless"
    } else {
        "  <== Intel NIC"
    };
    log::info!(
        "{}  {}:{}  {:02x}   {}  {}:{}  {:04x}  {}:{:08x}  {}{tag}",
        f.addr,
        f.vendor,
        f.device,
        f.revision,
        f.class,
        f.subsystem.0,
        f.subsystem.1,
        f.command,
        f.bar0.kind,
        f.bar0.base,
        f.class.name().unwrap_or(""),
    );
    match &f.csr {
        Some(Csr::Read { hw_rev, hw_if_config }) => {
            log::info!("           `- MMIO CSR read: HW_REV={hw_rev:#010x}  HW_IF_CONFIG={hw_if_config:#010x}");
        },
        Some(Csr::DecodeOff) => {
            log::info!("           `- MMIO CSR not read: memory decode is off (cmd {:#06x})", f.command);
        },
        Some(Csr::Failed(e)) => log::info!("           `- MMIO CSR read failed: {e}"),
        None => {},
    }
}
