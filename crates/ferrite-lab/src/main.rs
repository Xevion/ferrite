//! The Wi-Fi stack as a Linux process: the pinned Linux C linked against `ferrite-lkpi`, run until the
//! first kernel symbol nothing implements yet: a stub function, or a variable whose storage faults.
//!
//! The C runs on the shim's scheduler, as it will under UEFI. kthreadd and an init thread are kernel
//! threads; init checks a kthread round trip, then runs the initcalls in order, then the exitcalls. The
//! main stack only polls the scheduler until init is gone.

mod fence;
mod host;
mod kernel;
mod logger;

use std::backtrace::Backtrace;
use std::ffi::{CStr, c_int, c_void};
use std::io::Write as _;
use std::process::ExitCode;
use std::time::Duration;

use ferrite_lkpi::sched::{NoHooks, Scheduler};
use ferrite_wifi_sys::{exitcalls, initcalls};
use log::LevelFilter;

/// Exit status of a run that stopped in the halt hook, distinct from a crash or a clean exit.
const EXIT_HALTED: u8 = 86;
/// How long one poll step may run tasks before the main loop gets the CPU back.
const BUDGET: Duration = Duration::from_millis(10);

/// Print the stub that was hit and where from, then end the process.
fn halt(stub: &CStr) -> ! {
    let backtrace = Backtrace::force_capture();
    // The process ends right after; a failed write has nowhere to be reported.
    let _ = writeln!(std::io::stderr().lock(), "halted in stub {}\n{backtrace}", stub.to_string_lossy());
    std::process::exit(i32::from(EXIT_HALTED))
}

/// The init thread: the seam check, then every initcall and exitcall.
unsafe extern "C" fn init(_: *mut c_void) -> c_int {
    match kernel::kthread_round_trip() {
        Ok(()) => log::info!("kthread round trip through kthreadd: created, ran, stopped"),
        Err(error) => {
            log::error!("kthread round trip failed: {error}");
            std::process::exit(1);
        },
    }
    for call in initcalls() {
        log::info!("initcall level {} {}", call.level().to_string_lossy(), call.name().to_string_lossy());
        // SAFETY: init is the only caller, runs the calls one at a time, and the lab links the shim.
        let code = unsafe { call.call() };
        log::info!("initcall {} returned {code}", call.name().to_string_lossy());
    }
    for call in exitcalls() {
        log::info!("exitcall {}", call.name().to_string_lossy());
        // SAFETY: as above; every initcall returned, so each module is set up.
        unsafe { call.call() };
    }
    0
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

    let mut sched = Scheduler::new(host::HostPlatform::new(), host::MmapStacks, NoHooks);
    if let Err(error) = ferrite_lkpi::task::install(sched.handle(), kernel::tasks()) {
        log::error!("{error}");
        return ExitCode::FAILURE;
    }
    if let Err(code) = kernel::start_kthreadd() {
        log::error!("starting kthreadd failed: {code}");
        return ExitCode::FAILURE;
    }
    let init_task = match kernel::spawn(init, c"init", 0) {
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
    ExitCode::SUCCESS
}
