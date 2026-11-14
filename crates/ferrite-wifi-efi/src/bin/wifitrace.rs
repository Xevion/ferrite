//! `wifitrace`: the Linux Wi-Fi C under UEFI, started the way `ferrite-lab` starts it on the host.
//!
//! The shim comes up through `ferrite_wifi_efi::runtime` (kernel arena, task stacks, random seed, firmware
//! files, wall clock, the 1 ms timer and its stall report, the scheduler); the C runs on it as kernel
//! threads, polled from the main stack. Init runs the seam checks, every initcall, the firmware check, the
//! bring-up of a PCI Wi-Fi card and every exitcall (`ferrite-wifi-start`), and the app exits to the firmware
//! or the loader. Once a connection's port opens, the main loop runs IP over it (`ferrite_wifi_efi::ip`) for
//! the hold. A run that passes `RUN_LIMIT`, or stops in a stub, clears bus mastering on every card before
//! it exits.
//!
//! The lab's no-access fence over unimplemented kernel data has no UEFI counterpart yet: here that data is
//! plain zeroed storage in its own section.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::rc::Rc;
use core::time::Duration;

use ferrite_efi::bootlog::{BootLog, Echo};
use ferrite_efi::clock::Clock;
use ferrite_efi::cpu::{FpuControl, VectorSupport};
use ferrite_efi::{entropy, logger, session};
use ferrite_hal::Clock as _;
use ferrite_rt as _;
use ferrite_wifi::Shared;
use ferrite_wifi_efi::runtime::{EXIT_HALTED, NextPoll, Runtime, RuntimeConfig, halted};
use ferrite_wifi_start::StartUp;
use log::LevelFilter;
use uefi::prelude::*;

/// How long one poll step may run tasks before the main loop gets the CPU back.
const BUDGET: Duration = Duration::from_millis(10);
/// How long cfg80211 gets to load `regulatory.db` after its initcalls; FAT reads on real media are slow.
const REGDB_LIMIT: Duration = Duration::from_secs(5);
/// How long a card gets from probe to a registered interface: firmware read, INIT image, NVM.
const WIFI_LIMIT: Duration = Duration::from_secs(15);
/// How long a scan gets from its trigger to its end on the scan group; a full 8265 sweep takes a few seconds.
const SCAN_LIMIT: Duration = Duration::from_secs(10);
/// How long a connection gets from its request to the open port, and a disconnect to its event.
const CONNECT_LIMIT: Duration = Duration::from_secs(10);
/// How long the main loop's IP holds the link after the port opens: DHCP, router pings, then answering
/// pings and HTTP from the LAN.
const HOLD: Duration = Duration::from_secs(30);
/// How long init keeps the connection for the main loop at most.
const INIT_HOLD: Duration = Duration::from_secs(40);
/// The whole run ends here, wherever it is: start-up, a scan, a connect and its hold all fit.
const RUN_LIMIT: Duration = Duration::from_secs(60);
const EXIT_AFTER: Duration = Duration::from_secs(10);

#[entry]
fn main() -> Status {
    let fpu = FpuControl::read();
    let helpers = uefi::helpers::init();
    logger::init(BootLog::open("wifitrace", Echo::Console), LevelFilter::Info);
    if !fpu.sse_ready() {
        log::error!("cpu: the firmware left SSE disabled, stopping: {fpu}");
        logger::with(|log| session::finish(log, EXIT_AFTER));
        return Status::UNSUPPORTED;
    }
    log::info!("cpu: {fpu}; {}", VectorSupport::probe());
    if let Err(e) = helpers {
        log::warn!("uefi helpers init failed: {e}");
    }
    let status = run();
    logger::with(|log| session::finish(log, EXIT_AFTER));
    status
}

/// Bring the shim up, run the test script on it and poll until init is gone.
fn run() -> Status {
    let clock = Clock::start();
    let mut runtime = match Runtime::start(clock, &RuntimeConfig::default()) {
        Ok(runtime) => runtime,
        Err(e) => {
            log::error!("{e}");
            return e.status();
        },
    };
    let shared = Shared::new();
    let start_up = StartUp {
        sched: runtime.sched(),
        firmware_reference: runtime.firmware(),
        regdb_limit: REGDB_LIMIT,
        wifi_limit: WIFI_LIMIT,
        scan_limit: SCAN_LIMIT,
        connect_limit: CONNECT_LIMIT,
        hold: INIT_HOLD,
        shared: Rc::clone(&shared),
        lab_ap: None,
    };
    let init_task = match runtime.on_task(move || ferrite_wifi_start::start_init(start_up)) {
        Ok(Ok(task)) => task,
        Ok(Err(code)) => {
            log::error!("starting init failed: {code}");
            return Status::ABORTED;
        },
        Err(e) => {
            log::error!("starting init failed: {e}");
            return Status::ABORTED;
        },
    };

    let mut seed = [0_u8; 8];
    let seeded = entropy::firmware_bytes(&mut seed).inspect_err(|e| log::error!("smoltcp's seed: {e}"));
    let Ok(()) = seeded else { return Status::ABORTED };
    let mut ip = ferrite_wifi_efi::ip::MainNet::new(shared, u64::from_ne_bytes(seed), HOLD);
    let began = clock.now();
    let mut timed_out = false;
    while runtime.task_state(init_task).is_some() && halted().is_none() {
        if clock.now() - began >= RUN_LIMIT {
            log::error!(
                "run limit of {} s reached during {}; init is {:?}",
                RUN_LIMIT.as_secs(),
                ferrite_wifi_start::stage().to_string_lossy(),
                runtime.task_state(init_task)
            );
            timed_out = true;
            break;
        }
        let next = match runtime.poll(BUDGET) {
            Ok(next) => next,
            Err(e) => {
                log::error!("{e}");
                return Status::ABORTED;
            },
        };
        let ip_wait = ip.poll(clock.now());
        let init_gone = runtime.task_state(init_task).is_none();
        let wait = match next {
            NextPoll::Now => None,
            _ if init_gone || ip_wait == Some(Duration::ZERO) => None,
            NextPoll::After(wait) => Some(wait),
            NextPoll::Idle => {
                log::error!("every task is blocked and nothing will wake one; init never finished");
                return Status::ABORTED;
            },
        };
        if let Some(wait) = wait {
            runtime.idle(ip_wait.map_or(wait, |ip| ip.min(wait)));
        }
    }
    runtime.shutdown();

    if timed_out {
        return Status::TIMEOUT;
    }
    match halted() {
        Some(stub) => {
            log::error!("the run stopped in stub {}", stub.to_string_lossy());
            EXIT_HALTED
        },
        None if ferrite_wifi_start::init_failed() => Status::ABORTED,
        None => Status::SUCCESS,
    }
}
