#![no_std]
#![no_main]

use core::time::Duration;

use ferrite_efi::bootlog::{BootLog, Echo};
use ferrite_efi::cpu::{FpuControl, VectorState, VectorSupport};
use ferrite_efi::{logger, session};
use ferrite_mem::Caps;
#[cfg(ferrite_app)]
use ferrite_rt as _;
use ferrite_uefi::app::App;
use ferrite_uefi::diag;
use log::LevelFilter;
use uefi::prelude::*;

/// Nobody is waiting to read a report after a quit, so exit at once (or reset, under systemd-boot).
const EXIT_WAIT: Duration = Duration::ZERO;

#[entry]
fn main() -> Status {
    let fpu = FpuControl::read();
    // The log comes first so a hang or failure in anything below is recorded.
    logger::init(BootLog::open(diag::LOG_NAME, Echo::Silent), LevelFilter::Debug);
    diag::milestone("boot: entry reached");
    if !fpu.sse_ready() {
        log::error!("cpu: the firmware left SSE disabled, stopping: {fpu}");
        logger::with(|log| session::finish(log, EXIT_WAIT));
        return Status::UNSUPPORTED;
    }
    log::info!("cpu: {fpu}; {}", VectorSupport::probe());
    // Restored below on every return; a panic resets the machine instead.
    let vector = VectorState::enable()
        .inspect(|v| log::info!("cpu: vector state on, {v}"))
        .inspect_err(|e| log::warn!("cpu: {e}; sweeping with the baseline tiers"))
        .ok();
    let caps = match vector {
        Some(_) => {
            let support = VectorSupport::probe();
            // SAFETY: the probe reads CPUID and XCR0 as they are now, after enabling.
            unsafe { Caps::new(support.avx2_usable(), support.avx512f_usable()) }
        },
        // Without our state the wide tiers would run unguarded, so they stay off.
        None => Caps::BASELINE,
    };
    if let Err(e) = uefi::helpers::init() {
        log::warn!("boot: uefi helpers init failed: {e}");
    }

    let status = system::with_stdout(|output| {
        if let Err(e) = output.reset(false) {
            log::warn!("console: reset failed: {e}");
        }
        system::with_stdin(|input| match App::new(input, output, vector.as_ref(), caps) {
            Ok(mut app) => {
                app.run();
                Status::SUCCESS
            },
            Err(e) => {
                log::error!("ui: terminal setup failed: {e}");
                Status::DEVICE_ERROR
            },
        })
    });
    diag::milestone("exit");
    // Before `finish`, which may reset the machine.
    if let Some(vector) = vector {
        drop(vector);
        log::info!("cpu: vector state restored; {}", FpuControl::read());
    }
    logger::with(|log| session::finish(log, EXIT_WAIT));
    status
}
