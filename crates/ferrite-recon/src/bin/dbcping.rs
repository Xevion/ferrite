//! `dbcping`: bring up the xHCI Debug Capability on whichever controller the debug cable
//! reaches, then talk over it.
//!
//! The Thunderbolt controller at 3b:00.0 is opened first so DCE is set as early as
//! possible; the controller listing and register dump happen between polls while the far
//! host trains the link. Once configured it sends `ferrite dbcping: hello N` once a second
//! for 30 s and echoes any received bytes back as `rx: ...`.
//!
//! Everything is also written to `\EFI\ferrite\dbcping.txt`. With no keypress it gives up
//! after a timeout: launched by systemd-boot it cold resets (which also clears DbC),
//! otherwise it returns to the firmware.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::time::Duration;

use ferrite_efi::bootlog::{BootLog, Echo};
use ferrite_efi::clock::Clock;
use ferrite_efi::xhci::OpenDbc;
use ferrite_efi::{logger, session};
use ferrite_hal::Clock as _;
use ferrite_recon::bringup::{self, Link};
use ferrite_recon::summary;
use ferrite_xhci::Config;
use log::LevelFilter;
use uefi::prelude::*;

/// How long to keep the result on screen before exiting.
const EXIT_AFTER_SECS: u64 = 10;
/// How long to send hello lines once the link is up.
const PING: Duration = Duration::from_secs(30);
const HELLO_EVERY: Duration = Duration::from_secs(1);
/// The console is only checked for a key this often.
const KEY_EVERY: Duration = Duration::from_millis(100);

#[entry]
fn main() -> Status {
    let helpers = uefi::helpers::init();
    logger::init(BootLog::open("dbcping", Echo::Console), LevelFilter::Debug);
    if let Err(e) = helpers {
        log::warn!("uefi helpers init failed: {e}");
    }
    let clock = logger::with(|log| log.clock()).unwrap_or_else(Clock::start);
    let start = clock.now();
    log::info!("ferrite-uefi :: dbcping - xHCI Debug Capability bring-up");

    let link = bringup::bring_up(clock, start, &Config::default()).map(|mut link| {
        ping(&mut link, clock);
        summary::report(&mut link.dbc, start);
        link
    });

    // An unattended run cold-resets inside `finish`, which also clears DbC, so the link
    // stays enabled until then. Dropping it afterwards clears DCE before the memory goes.
    logger::with(|log| session::finish(log, EXIT_AFTER_SECS));
    drop(link);
    Status::SUCCESS
}

/// Send a hello line each second and echo whatever arrives, for [`PING`].
fn ping(link: &mut Link, clock: Clock) {
    let Link { dbc, survey } = link;
    let begin = clock.now();
    let mut next_hello = begin;
    let mut next_key = begin;
    let mut hellos = 0u32;
    let mut got = Vec::new();
    loop {
        let now = clock.now();
        if now - begin >= PING {
            break;
        }
        if now >= next_key {
            next_key = now + KEY_EVERY;
            if key_pressed() {
                break;
            }
        }

        got.clear();
        dbc.poll(|bytes| got.extend_from_slice(bytes));
        if !got.is_empty() {
            let text = String::from_utf8_lossy(&got).into_owned();
            let accepted = dbc.write(format!("rx: {}\n", text.trim_end()).as_bytes()).accepted;
            log::info!("received {} byte(s): {text:?}; echoed ({accepted} bytes queued)", got.len());
        }

        if now >= next_hello {
            hellos += 1;
            next_hello = next_hello + HELLO_EVERY;
            hello(dbc, hellos, now - begin);
            logger::with(BootLog::poll);
        }
        if survey.pending() {
            survey.step(dbc);
        }
    }
}

fn hello(dbc: &mut OpenDbc, n: u32, elapsed: Duration) {
    let accepted = dbc.write(format!("ferrite dbcping: hello {n}\n").as_bytes()).accepted;
    let snap = dbc.snapshot();
    let stats = dbc.stats();
    log::info!(
        "t={}s state={} hello {n} queued={accepted} DCCTRL={:#010x} DCPORTSC={:#010x} sent={} rx={} events={} buffered={}",
        elapsed.as_secs(),
        dbc.state(),
        snap.ctrl.raw_value(),
        snap.portsc.raw_value(),
        stats.bytes_sent,
        stats.bytes_received,
        stats.events,
        dbc.buffered(),
    );
}

fn key_pressed() -> bool {
    system::with_stdin(|input| matches!(input.read_key(), Ok(Some(_))))
}
