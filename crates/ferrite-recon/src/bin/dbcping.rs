//! `dbcping`: bring up the xHCI Debug Capability on whichever controller the
//! debug cable reaches, then talk over it.
//!
//! Each xHCI with a DbC is tried in turn (the Thunderbolt controller at
//! 3b:00.0 first): program the DbC, set DCE, and wait for the far host to
//! configure it. On success it sends `ferrite dbcping: hello N` once a second
//! for 30 s and echoes any received bytes back as `rx: ...`. A controller that
//! never configures gets DCE cleared and the next one is tried.
//!
//! Everything is also written to `\EFI\ferrite\dbcping.txt`, flushed on every
//! line, so a hang still leaves the register timeline on the boot volume. With
//! no keypress it gives up after a timeout: launched by systemd-boot it cold
//! resets (which also clears DbC), otherwise it returns to the firmware.
//!
//! Build/run:  `just dbcping`  (graphical)  /  `just dbcping-serial`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::time::Duration;

use ferrite_efi::bootlog::{BootLog, Echo};
use ferrite_efi::clock::Clock;
use ferrite_efi::dma::iommu_handle_count;
use ferrite_efi::session;
use ferrite_efi::xhci::{self, Controller, OpenDbc};
use ferrite_xhci::{Config, State, XhciDevice};
use uefi::prelude::*;

/// How long to keep the result on screen before exiting.
const EXIT_AFTER_SECS: u64 = 10;
/// How long to send hello lines once the link is up.
const PING_SECS: u64 = 30;
/// Controller tried first when several have a DbC (the Thunderbolt xHCI on the X1 Carbon).
const PREFERRED_BDF: (u8, u8, u8) = (0x3b, 0x00, 0);

#[entry]
fn main() -> Status {
    uefi::helpers::init().unwrap();
    let mut report = BootLog::open("dbcping", Echo::Console);
    report.line("ferrite-uefi :: dbcping  -  xHCI Debug Capability bring-up");

    let link = run(&mut report);

    // An unattended run cold-resets inside `finish`, which also clears DbC, so
    // the link stays enabled until then.
    session::finish(&mut report, EXIT_AFTER_SECS);
    // Returning to the firmware: dropping the driver clears DCE so the
    // controller stops using memory we are about to give back.
    drop(link);
    Status::SUCCESS
}

/// Find controllers, bring up the first DbC that the far host configures, and
/// ping over it. Returns the live link so the caller decides when to tear it down.
fn run(report: &mut BootLog) -> Option<OpenDbc> {
    report.line(&format!("IOMMU protocol handles: {:?}", iommu_handle_count()));

    let clock = Clock::start();
    let scan = match xhci::find_xhcis() {
        Ok(scan) => scan,
        Err(e) => {
            report.line(&format!("PCI scan failed: {e}"));
            return None;
        },
    };
    for failure in &scan.failures {
        report.line(&format!("PCI scan incomplete: {failure}"));
    }
    let mut devs: Vec<(Controller, XhciDevice)> = Vec::new();
    for ctrl in scan.controllers {
        match ctrl.probe() {
            Ok(dev) => devs.push((ctrl, dev)),
            Err(e) => report.line(&format!("{}: probe failed: {e}", ctrl.function.addr())),
        }
    }
    devs.sort_by_key(|(_, d)| (d.dbc_regs().is_none(), bdf(d) != PREFERRED_BDF));
    report.line(&format!("{} xHCI controller(s)", devs.len()));
    for (_, dev) in &devs {
        describe(report, dev);
    }

    let cfg = Config::default();
    for (ctrl, dev) in devs.iter().filter(|(_, d)| d.dbc_regs().is_some()) {
        report.line(&format!("\n== trying {dev} (timeout {} ms) ==", cfg.timeout.as_millis()));
        if let Ok(Some(before)) = ctrl.snapshot(dev) {
            report.line("registers before:");
            report.line(&format!("{before}"));
        }

        let mut dbc = match xhci::open(ctrl, &cfg, clock) {
            Ok(dbc) => dbc,
            Err(e) => {
                report.line(&format!("open failed: {e}"));
                continue;
            },
        };

        match dbc.wait_configured() {
            Ok(waited) => {
                report.line(&format!("CONFIGURED after {} ms", waited.as_millis()));
                ping(&mut dbc, clock, report);
                summarize(report, &mut dbc);
                return Some(dbc);
            },
            Err(e) => {
                report.line(&format!("FAILED: {e}"));
                summarize(report, &mut dbc);
                if let Err(e) = dbc.disable() {
                    report.line(&format!("disable failed: {e}"));
                }
            },
        }
    }

    if devs.iter().all(|(_, d)| d.dbc_regs().is_none()) {
        report.line("no xHCI with a Debug Capability (QEMU's qemu-xhci has none)");
    }
    None
}

fn bdf(dev: &XhciDevice) -> (u8, u8, u8) {
    let a = dev.addr();
    (a.bus(), a.device(), a.function())
}

fn describe(report: &mut BootLog, dev: &XhciDevice) {
    report.line(&format!(
        "  {dev}  mmio {:#x}  caplength {:#x}  ports {}  dbc {}  legacy {}  usb3 ranges {:?}",
        dev.mmio(),
        dev.cap_length(),
        dev.max_ports(),
        dev.dbc_regs().map_or(String::from("none"), |a| format!("{a:#x}")),
        dev.legacy().map_or(String::from("none"), |a| format!("{a:#x}")),
        dev.usb3_ports(),
    ));
}

/// Send a hello line each second and echo whatever arrives, for `PING_SECS`.
fn ping(dbc: &mut OpenDbc, clock: Clock, report: &mut BootLog) {
    let start = clock.ms();
    let mut next_hello_ms = 0;
    let mut n = 0u32;
    loop {
        let elapsed = clock.ms() - start;
        if elapsed >= PING_SECS * 1000 || key_pressed() {
            break;
        }

        let mut got = Vec::new();
        dbc.poll(|bytes| got.extend_from_slice(bytes));
        if !got.is_empty() {
            let text = String::from_utf8_lossy(&got).into_owned();
            report.line(&format!("received {} byte(s): {:?}", got.len(), text));
            let accepted = dbc.write(format!("rx: {}\n", text.trim_end()).as_bytes()).accepted;
            report.line(&format!("echoed ({accepted} bytes queued)"));
        }

        if elapsed >= next_hello_ms {
            n += 1;
            next_hello_ms += 1000;
            let accepted = dbc.write(format!("ferrite dbcping: hello {n}\n").as_bytes()).accepted;
            let snap = dbc.snapshot();
            let stats = dbc.stats();
            report.line(&format!(
                "t={}s state={} hello {n} queued={accepted} DCCTRL={:#010x} DCPORTSC={:#010x} sent={} rx={} events={} buffered={}",
                elapsed / 1000,
                dbc.state(),
                snap.ctrl.raw_value(),
                snap.portsc.raw_value(),
                stats.bytes_sent,
                stats.bytes_received,
                stats.events,
                dbc.buffered(),
            ));
        }
        boot::stall(Duration::from_millis(2));
    }
}

/// Registers, timings, counters and the event types seen.
fn summarize(report: &mut BootLog, dbc: &mut OpenDbc) {
    report.line("registers after:");
    let snap = dbc.snapshot();
    report.line(&format!("{snap}"));
    if let Some(port) = dbc.debug_port() {
        let portsc = dbc.host_portsc(port).raw_value();
        report.line(&format!("  host PORTSC[{port}]={portsc:#010x}"));
    }
    let t = dbc.timings();
    report.line(&format!(
        "state {} ; timings ms: dce_ready={:?} connected={:?} configured={:?}",
        dbc.state(),
        t.dce_ready.map(|i| i.since_epoch().as_millis()),
        t.connected.map(|i| i.since_epoch().as_millis()),
        t.configured.map(|i| i.since_epoch().as_millis())
    ));
    let s = dbc.stats().clone();
    report.line(&format!(
        "events {} (out_done {} in_done {} errors {} last_cc {} unknown {}) port_changes {} halts {} rearms {} kicks {}",
        s.events,
        s.out_done,
        s.in_done,
        s.transfer_errors,
        s.last_error_cc,
        s.unknown_transfer_events,
        s.port_changes,
        s.halts,
        s.rearms,
        s.kicks
    ));
    report.line(&format!(
        "bytes queued {} sent {} received {} dropped {}",
        s.bytes_queued, s.bytes_sent, s.bytes_received, s.bytes_dropped
    ));
    for (ty, name, count) in s.event_type_counts() {
        report.line(&format!("  event type {ty} ({name}): {count}"));
    }
    if let Some(fault) = dbc.last_fault() {
        report.line(&format!("DbC is {}: {fault}", State::Faulted));
    }
}

fn key_pressed() -> bool {
    system::with_stdin(|input| matches!(input.read_key(), Ok(Some(_))))
}
