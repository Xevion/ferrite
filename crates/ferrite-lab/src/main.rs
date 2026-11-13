//! The Wi-Fi stack as a Linux process: the pinned Linux C linked against `ferrite-lkpi`, run until the
//! first kernel symbol nothing implements yet (a stub function, or a variable whose storage faults), or
//! to the end of the exitcalls.
//!
//! The C runs on the shim's scheduler, as it will under UEFI. kthreadd and an init thread are kernel
//! threads; init (`ferrite-wifi-start`, shared with the UEFI image) checks a kthread round trip, runs
//! `lx_kernel_init`, checks a sleep and a timer, runs the initcalls in order, checks firmware loading,
//! then runs the exitcalls. The main stack only polls the scheduler until init is gone, sleeping until
//! the timer deadline when all are parked.
mod fence;
mod host;
mod logger;

use std::backtrace::Backtrace;
use std::ffi::CStr;
use std::io::Write as _;
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use ferrite_lkpi::sched::{NoHooks, Scheduler, Step};
use ferrite_wifi_start::StartUp;
use log::LevelFilter;

/// Exit status of a run that stopped in the halt hook, distinct from a crash or a clean exit.
const EXIT_HALTED: u8 = 86;
/// Where the lab finds firmware files.
const FIRMWARE_DIR: &str = "/lib/firmware";
/// How long cfg80211 gets to load `regulatory.db` after its initcalls.
const REGDB_LIMIT: Duration = Duration::from_secs(2);
/// How long one poll step may run tasks before the main loop gets the CPU back.
const BUDGET: Duration = Duration::from_millis(10);
/// The kernel's memory: every page, slab object and vmalloc block.
const ARENA_SIZE: usize = 256 << 20;

/// Print the stub that was hit and where from, then end the process.
fn halt(stub: &CStr) -> ! {
    let backtrace = Backtrace::force_capture();
    // The process ends right after; a failed write has nowhere to be reported.
    let _ = writeln!(std::io::stderr().lock(), "halted in stub {}\n{backtrace}", stub.to_string_lossy());
    std::process::exit(i32::from(EXIT_HALTED))
}

fn main() -> ExitCode {
    logger::init(LevelFilter::Info);
    if let Err(error) = ferrite_lkpi::set_halt_hook(halt) {
        log::error!("{error}");
        return ExitCode::FAILURE;
    }
    if let Err(error) = fence::install() {
        log::error!("{error}");
        return ExitCode::FAILURE;
    }

    match host::arena(ARENA_SIZE).map(ferrite_lkpi::mem::install) {
        Ok(Ok(())) => {},
        Ok(Err(error)) => {
            log::error!("{error}");
            return ExitCode::FAILURE;
        },
        Err(error) => {
            log::error!("{error}");
            return ExitCode::FAILURE;
        },
    }

    match host::entropy().map(ferrite_lkpi::random::seed) {
        Ok(Ok(())) => {},
        Ok(Err(error)) => {
            log::error!("{error}");
            return ExitCode::FAILURE;
        },
        Err(error) => {
            log::error!("reading host entropy failed: {error}");
            return ExitCode::FAILURE;
        },
    }

    let firmware: &'static host::FirmwareDir = Box::leak(Box::new(host::FirmwareDir::new(FIRMWARE_DIR)));
    if let Err(error) = ferrite_lkpi::firmware::install(firmware) {
        log::error!("{error}");
        return ExitCode::FAILURE;
    }

    let wall = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    if let Err(error) = ferrite_lkpi::time::set_boot_wall_clock(wall) {
        log::error!("{error}");
        return ExitCode::FAILURE;
    }

    let mut sched = Scheduler::new(host::HostPlatform::new(), host::MmapStacks, NoHooks);
    if let Err(error) = ferrite_lkpi::task::install(sched.handle(), ferrite_wifi_start::tasks()) {
        log::error!("{error}");
        return ExitCode::FAILURE;
    }
    if let Err(code) = ferrite_wifi_start::start_kthreadd() {
        log::error!("starting kthreadd failed: {code}");
        return ExitCode::FAILURE;
    }
    let start_up = StartUp { sched: sched.handle(), firmware_reference: firmware, regdb_limit: REGDB_LIMIT };
    let init_task = match ferrite_wifi_start::start_init(start_up) {
        Ok(task) => task,
        Err(code) => {
            log::error!("starting init failed: {code}");
            return ExitCode::FAILURE;
        },
    };
    let Some(init_id) = sched.find_by_user(init_task.as_ptr() as usize) else {
        log::error!("init has no scheduler task");
        return ExitCode::FAILURE;
    };

    while sched.info(init_id).is_some() {
        match sched.poll(BUDGET) {
            Ok(step) if step.runnable || sched.info(init_id).is_none() => {},
            Ok(Step { next_deadline: Some(at), .. }) => {
                std::thread::sleep(at.saturating_duration_since(sched.handle().now()));
            },
            Ok(_) => {
                log::error!("every task is blocked and nothing will wake one; init never finished");
                return ExitCode::FAILURE;
            },
            Err(error) => {
                log::error!("{error}");
                return ExitCode::FAILURE;
            },
        }
    }
    if ferrite_wifi_start::init_failed() { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}
