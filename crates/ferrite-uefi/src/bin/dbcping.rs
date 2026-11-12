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

use ferrite_uefi::dbc::clock::Clock;
use ferrite_uefi::dbc::discover::{XhciDevice, find_xhcis};
use ferrite_uefi::dbc::dma::iommu_handle_count;
use ferrite_uefi::dbc::regs::trb_type_name;
use ferrite_uefi::dbc::{Config, Dbc, State, snapshot_device};
use uefi::boot::{self, EventType, TimerTrigger, Tpl};
use uefi::fs::FileSystem;
use uefi::prelude::*;
use uefi::println;
use uefi::runtime::{self, ResetType, VariableVendor};
use uefi::{CStr16, cstr16, guid};

const REPORT_DIR: &CStr16 = cstr16!("\\EFI\\ferrite");
const REPORT_PATH: &CStr16 = cstr16!("\\EFI\\ferrite\\dbcping.txt");
/// How long to keep the result on screen before exiting.
const EXIT_AFTER_SECS: u64 = 10;
/// How long to send hello lines once the link is up.
const PING_SECS: u64 = 30;
/// Controller tried first when several have a DbC (the Thunderbolt xHCI on the X1 Carbon).
const PREFERRED_BDF: (u8, u8, u8) = (0x3b, 0x00, 0);

/// Report text mirrored to the console and to a file rewritten on every line.
struct Report {
    fs: Option<FileSystem>,
    text: String,
}

impl Report {
    fn open() -> Self {
        let fs = boot::get_image_file_system(boot::image_handle())
            .ok()
            .map(FileSystem::new);
        let mut report = Self {
            fs,
            text: String::new(),
        };
        if let Some(fs) = report.fs.as_mut() {
            let _ = fs.create_dir_all(REPORT_DIR);
        }
        report
    }

    fn line(&mut self, s: &str) {
        self.text.push_str(s);
        self.text.push('\n');
        println!("{s}");
        self.flush();
    }

    fn flush(&mut self) -> bool {
        self.fs
            .as_mut()
            .is_some_and(|fs| fs.write(REPORT_PATH, self.text.as_bytes()).is_ok())
    }
}

#[entry]
fn main() -> Status {
    uefi::helpers::init().unwrap();
    let mut report = Report::open();
    report.line("ferrite-uefi :: dbcping  -  xHCI Debug Capability bring-up");

    let link = run(&mut report);

    let saved = report.flush();
    println!(
        "\nreport file: {}\npress a key to exit (auto-exit in {EXIT_AFTER_SECS}s)...",
        if saved {
            "\\EFI\\ferrite\\dbcping.txt"
        } else {
            "NOT written"
        }
    );
    // An unattended one-shot run resets instead of returning, so the machine
    // lands back in its default OS. The reset also clears DbC, which is why it
    // is left enabled until then.
    if !wait_for_key_or_timeout(EXIT_AFTER_SECS) && launched_by_systemd_boot() {
        runtime::reset(ResetType::COLD, Status::SUCCESS, None);
    }
    // Returning to the firmware: dropping the driver clears DCE so the
    // controller stops using memory we are about to give back.
    drop(link);
    Status::SUCCESS
}

/// Find controllers, bring up the first DbC that the far host configures, and
/// ping over it. Returns the live link so the caller decides when to tear it down.
fn run(report: &mut Report) -> Option<Dbc> {
    let iommu = iommu_handle_count();
    report.line(&format!(
        "IOMMU protocol handles: {iommu} ({})",
        if iommu == 0 {
            "identity DMA assumed safe"
        } else {
            "DMA may need mapping"
        }
    ));

    let clock = Clock::start();
    let mut devs = find_xhcis();
    devs.sort_by_key(|d| (d.dbc.is_none(), d.bdf() != PREFERRED_BDF));
    report.line(&format!("{} xHCI controller(s)", devs.len()));
    for dev in &devs {
        describe(report, dev);
    }

    let cfg = Config::default();
    for dev in devs.iter().filter(|d| d.dbc.is_some()) {
        report.line(&format!(
            "\n== trying {dev} (timeout {} ms) ==",
            cfg.timeout_ms
        ));
        if let Some(before) = snapshot_device(dev) {
            report.line("registers before:");
            for l in before.lines() {
                report.line(&format!("  {l}"));
            }
        }

        let opened = {
            let mut note = |s: &str| report.line(s);
            Dbc::open(dev, &cfg, clock, &mut note)
        };
        let mut dbc = match opened {
            Ok(dbc) => dbc,
            Err(e) => {
                report.line(&format!("open failed: {e}"));
                continue;
            }
        };

        let waited = {
            let mut note = |s: &str| report.line(s);
            dbc.wait_configured(&mut note)
        };
        match waited {
            Ok(ms) => {
                report.line(&format!("CONFIGURED after {ms} ms"));
                ping(&mut dbc, clock, report);
                summarize(report, &mut dbc);
                return Some(dbc);
            }
            Err(e) => {
                report.line(&format!("FAILED: {e}"));
                summarize(report, &mut dbc);
                dbc.disable();
            }
        }
    }

    if devs.iter().all(|d| d.dbc.is_none()) {
        report.line("no xHCI with a Debug Capability (QEMU's qemu-xhci has none)");
    }
    None
}

fn describe(report: &mut Report, dev: &XhciDevice) {
    report.line(&format!(
        "  {dev}  mmio {:#x}  caplength {:#x}  ports {}  dbc {}  legacy {}  usb3 ranges {:?}",
        dev.mmio,
        dev.cap_length,
        dev.max_ports,
        dev.dbc.map_or(String::from("none"), |a| format!("{a:#x}")),
        dev.legacy
            .map_or(String::from("none"), |a| format!("{a:#x}")),
        dev.usb3_ports,
    ));
}

/// Send a hello line each second and echo whatever arrives, for `PING_SECS`.
fn ping(dbc: &mut Dbc, clock: Clock, report: &mut Report) {
    let start = clock.ms();
    let mut next_hello_ms = 0;
    let mut n = 0u32;
    loop {
        let elapsed = clock.ms() - start;
        if elapsed >= PING_SECS * 1000 || key_pressed() {
            break;
        }

        let mut got = Vec::new();
        {
            let mut note = |s: &str| report.line(s);
            dbc.poll(&mut |bytes| got.extend_from_slice(bytes), &mut note);
        }
        if !got.is_empty() {
            let text = String::from_utf8_lossy(&got).into_owned();
            report.line(&format!("received {} byte(s): {:?}", got.len(), text));
            let accepted = dbc.write(format!("rx: {}\n", text.trim_end()).as_bytes());
            report.line(&format!("echoed ({accepted} bytes queued)"));
        }

        if elapsed >= next_hello_ms {
            n += 1;
            next_hello_ms += 1000;
            let accepted = dbc.write(format!("ferrite dbcping: hello {n}\n").as_bytes());
            let snap = dbc.snapshot();
            let stats = dbc.stats();
            report.line(&format!(
                "t={}s state={} hello {n} queued={accepted} DCCTRL={:#010x} DCPORTSC={:#010x} sent={} rx={} events={} buffered={}",
                elapsed / 1000,
                dbc.state(),
                snap.ctrl,
                snap.portsc,
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
fn summarize(report: &mut Report, dbc: &mut Dbc) {
    report.line("registers after:");
    for l in dbc.snapshot().lines() {
        report.line(&format!("  {l}"));
    }
    let port = dbc.debug_port();
    if port != 0 {
        let portsc = dbc.host_portsc(port);
        report.line(&format!("  host PORTSC[{port}]={portsc:#010x}"));
    }
    let t = dbc.timings();
    report.line(&format!(
        "state {} ; timings ms: dce_ready={:?} connected={:?} configured={:?}",
        dbc.state(),
        t.dce_ready_ms,
        t.connected_ms,
        t.configured_ms
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
    for (ty, &count) in s.event_types.iter().enumerate().filter(|&(_, &c)| c > 0) {
        report.line(&format!(
            "  event type {ty} ({}): {count}",
            trb_type_name(ty as u32)
        ));
    }
    if dbc.state() == State::Faulted {
        report.line("DbC is faulted");
    }
}

fn key_pressed() -> bool {
    system::with_stdin(|input| matches!(input.read_key(), Ok(Some(_))))
}

/// True when systemd-boot chainloaded us: it publishes `LoaderInfo` before
/// starting an entry, and nothing else sets that variable.
fn launched_by_systemd_boot() -> bool {
    let vendor = VariableVendor(guid!("4a67b082-0a4c-41cf-b6c7-440b29bb8c4f"));
    runtime::variable_exists(cstr16!("LoaderInfo"), &vendor).unwrap_or(false)
}

/// Wait for a keypress, but give up after `secs` so unattended runs still exit.
/// Returns whether a key was pressed.
fn wait_for_key_or_timeout(secs: u64) -> bool {
    let stall = || boot::stall(Duration::from_secs(secs));
    let timer = unsafe { boot::create_event(EventType::TIMER, Tpl::APPLICATION, None, None) };
    let Ok(timer) = timer else {
        stall();
        return false;
    };
    if boot::set_timer(&timer, TimerTrigger::Relative(Duration::from_secs(secs))).is_err() {
        stall();
        return false;
    }
    match system::with_stdin(|input| input.wait_for_key_event().ok()) {
        Some(key) => matches!(boot::wait_for_event(&mut [key, timer]), Ok(0)),
        None => {
            let _ = boot::wait_for_event(&mut [timer]);
            false
        }
    }
}
