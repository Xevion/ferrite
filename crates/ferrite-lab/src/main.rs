//! The Wi-Fi stack as a Linux process: the pinned Linux C linked against `ferrite-lkpi`, run until the
//! first kernel symbol nothing implements yet: a stub function, or a variable whose storage faults.

mod fence;
mod logger;

use std::backtrace::Backtrace;
use std::ffi::CStr;
use std::io::Write as _;
use std::process::ExitCode;

use ferrite_wifi_sys::{exitcalls, initcalls};
use log::LevelFilter;

/// Exit status of a run that stopped in the halt hook, distinct from a crash or a clean exit.
const EXIT_HALTED: u8 = 86;

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

    for call in initcalls() {
        log::info!("initcall level {} {}", call.level().to_string_lossy(), call.name().to_string_lossy());
        // SAFETY: the lab is the only caller, runs the calls one at a time, and links the shim they need.
        let code = unsafe { call.call() };
        log::info!("initcall {} returned {code}", call.name().to_string_lossy());
    }
    for call in exitcalls() {
        log::info!("exitcall {}", call.name().to_string_lossy());
        // SAFETY: as above; every initcall returned, so each module is set up.
        unsafe { call.call() };
    }
    ExitCode::SUCCESS
}
