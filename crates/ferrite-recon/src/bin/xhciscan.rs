//! `xhciscan`: find xHCI USB host controllers, map their MMIO, and report what we can do
//! with their ports from bare metal. This is the recon for host-to-host USB communication:
//!
//!   - PCI command bits and power state as the firmware left them.
//!   - Does the controller support Port Power Control (HCCPARAMS1.PPC)? If so, `PORTSC.PP`
//!     toggles VBUS on a port from software, a one-bit transmit line over any cable.
//!   - Is the Debug Capability (DbC) present, and what do its live registers say? DbC is
//!     the bidirectional USB serial channel.
//!   - Per-port live state: connected, powered, link state, speed.
//!
//! Every xHCI on every root bridge is reported, because a laptop can have more than one
//! (a PCH controller plus a Thunderbolt one) and only some of them expose a usable DbC
//! port. Nothing is written to config space: a controller whose memory decode is off is
//! reported as such and its MMIO is left alone.
//!
//! The report goes to the console and `\EFI\ferrite\xhciscan.txt`. With no keypress it
//! gives up after a timeout: launched by systemd-boot it reboots, otherwise it returns to
//! the firmware. Runs on real hardware and under QEMU (`-device qemu-xhci`).

#![no_std]
#![no_main]

extern crate alloc;

use ferrite_efi::bootlog::{BootLog, Echo};
use ferrite_efi::pci::RootBridge;
use ferrite_efi::xhci::{self, Controller};
use ferrite_efi::{logger, session};
use ferrite_hal::{Mmio, MmioExt, ReadOnly, Reg};
use ferrite_recon::caps;
use ferrite_recon::display::{Chain, yes_no};
use ferrite_recon::pci::{DeviceId, VendorId, power_name};
use ferrite_xhci::XhciDevice;
use ferrite_xhci::pci::{CMD_BUS_MASTER, CMD_MEMORY};
use ferrite_xhci::regs::{PortSc, cap, xcap};
use log::LevelFilter;
use uefi::prelude::*;

/// How long to keep the report on screen before exiting back to the boot manager.
const EXIT_AFTER_SECS: u64 = 20;

/// HCCPARAMS1 bit: the controller can switch port power.
const HCC1_PORT_POWER_CONTROL: u32 = 1 << 3;

/// Root-port PORTSC registers start here from the operational base, one block per port.
/// `ferrite_xhci::regs::host_portsc` only builds them for USB 3 ports, and this lists all.
const PORTSC_BASE: u64 = 0x400;
const PORTSC_STRIDE: u64 = 0x10;

#[entry]
fn main() -> Status {
    let helpers = uefi::helpers::init();
    logger::init(BootLog::open("xhciscan", Echo::Console), LevelFilter::Debug);
    if let Err(e) = helpers {
        log::warn!("uefi helpers init failed: {e}");
    }
    report();
    logger::with(|log| session::finish(log, EXIT_AFTER_SECS));
    Status::SUCCESS
}

/// Enumerate every xHCI controller and log a human-readable report.
fn report() {
    log::info!("ferrite-uefi :: xhciscan - USB host controller recon");
    log::info!("");

    let scan = match xhci::find_xhcis() {
        Ok(scan) => scan,
        Err(e) => {
            log::error!("no PCI root bridge protocol available: {}", Chain(&e));
            return;
        },
    };
    for failure in &scan.failures {
        log::warn!("PCI scan incomplete: {}", Chain(failure));
    }
    if scan.controllers.is_empty() {
        log::info!("no xHCI controller found (add `-device qemu-xhci` in QEMU)");
        return;
    }
    for (i, ctrl) in scan.controllers.iter().enumerate() {
        if i > 0 {
            log::info!("");
        }
        report_controller(ctrl);
    }
    log::info!("");
    log::info!("{} xHCI controller(s) reported", scan.controllers.len());
}

/// What config space says, then the capability registers, ports and extended capabilities.
fn report_controller(ctrl: &Controller) {
    let f = &ctrl.function;
    log::info!("xHCI at {}  {}:{}", f.addr(), VendorId(f.vendor()), DeviceId(f.device()));
    log::info!("  MMIO base (BAR0): {:#012x}", f.mmio());
    log::info!(
        "  PCI command {:#06x}: memory={} bus-master={}   power state: {}",
        f.command(),
        yes_no(f.command() & CMD_MEMORY != 0),
        yes_no(f.command() & CMD_BUS_MASTER != 0),
        power_name(f.power()),
    );
    if !f.decoding() {
        log::warn!("  memory decode is off or the function is not in D0: MMIO not read");
        return;
    }
    let dev = match ctrl.probe() {
        Ok(dev) => dev,
        Err(e) => {
            log::warn!("  probe failed: {}", Chain(&e));
            return;
        },
    };
    let mut rb = match RootBridge::open(ctrl.bridge) {
        Ok(rb) => rb,
        Err(e) => {
            log::warn!("  opening the root bridge failed: {e}");
            return;
        },
    };

    let mmio = dev.mmio();
    let version = rb.read(mmio, cap::CAPLENGTH).raw_value() >> 16;
    let params = rb.read(mmio, cap::HCCPARAMS1);
    log::info!(
        "  HCIVERSION {:x}.{:02x}   CAPLENGTH {:#x}   MaxPorts {}",
        version >> 8,
        version & 0xff,
        dev.cap_length(),
        dev.max_ports()
    );
    let ppc = params.raw_value() & HCC1_PORT_POWER_CONTROL != 0;
    log::info!(
        "  Port Power Control (PPC): {}   {}",
        yes_no(ppc),
        if ppc {
            "=> PORTSC.PP can toggle VBUS from software (1-bit TX line)"
        } else {
            "=> port power is fixed; no software VBUS toggle"
        }
    );

    log::info!("  ports:");
    for n in 1..=dev.max_ports() {
        let reg = Reg::<PortSc, ReadOnly>::new(PORTSC_BASE + u64::from(n - 1) * PORTSC_STRIDE);
        if let Ok(portsc) = rb.try_read(dev.op_base(), reg) {
            log::info!(
                "    port {n:>2}: PORTSC={:#010x}  connected={}  enabled={}  powered={}  link={}  speed={}",
                portsc.raw_value(),
                yes_no(portsc.ccs()),
                yes_no(portsc.ped()),
                yes_no(portsc.pp()),
                portsc.link(),
                portsc.speed_code(),
            );
        }
    }

    report_ext_caps(ctrl, &dev, &mut rb, params.xecp());
}

fn report_ext_caps(ctrl: &Controller, dev: &XhciDevice, rb: &mut RootBridge, xecp: u16) {
    if xecp == 0 {
        log::info!("  extended caps: none");
        return;
    }
    let mut dbc_found = false;
    for cap in caps::walk(rb, dev.mmio(), xecp) {
        let at = cap.at;
        match cap.header.id() {
            xcap::ID_DEBUG => {
                dbc_found = true;
                log::info!("  ext cap @ {at:#x}: id 0x0a DEBUG CAPABILITY (DbC) <== USB-serial channel!");
                match ctrl.snapshot(dev) {
                    Ok(Some(snap)) => log::info!("{snap}"),
                    Ok(None) => {},
                    Err(e) => log::warn!("    DbC registers not read: {}", Chain(&e)),
                }
            },
            xcap::ID_SUPPORTED_PROTOCOL => log::info!("{}", protocol_line(rb, cap)),
            other => log::info!("  ext cap @ {at:#x}: id {other:#04x}"),
        }
    }
    log::info!(
        "  Debug Capability (DbC): {}   {}",
        yes_no(dbc_found),
        if dbc_found {
            "=> a real bidirectional USB-serial link is possible (crossover cable)"
        } else {
            "=> not exposed here (QEMU's model omits it; real HW should have it)"
        }
    );
}

/// The Supported Protocol capability: its name string (`USB `) and revision.
fn protocol_line(rb: &mut RootBridge, cap: caps::ExtCap) -> alloc::string::String {
    let minor = (cap.header.raw_value() >> 16) & 0xff;
    let name = rb.read32(cap.at.offset(4)).to_le_bytes().map(|b| if b.is_ascii_graphic() { b as char } else { '.' });
    alloc::format!(
        "  ext cap @ {:#x}: id 0x02 supported protocol {}{}{}{} {}.{minor}",
        cap.at,
        name[0],
        name[1],
        name[2],
        name[3],
        cap.header.major_revision(),
    )
}
