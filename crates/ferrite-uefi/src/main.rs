#![no_std]
#![no_main]

use core::time::Duration;

use ferrite_efi::bootlog::{BootLog, Echo};
use ferrite_efi::{logger, session};
use ferrite_uefi::app::App;
use ferrite_uefi::diag;
use log::LevelFilter;
use uefi::prelude::*;

/// Nobody is waiting to read a report after a quit, so exit at once (or reset, under systemd-boot).
const EXIT_WAIT: Duration = Duration::ZERO;

#[entry]
fn main() -> Status {
    // The log comes first so a hang or failure in anything below is recorded.
    logger::init(BootLog::open(diag::LOG_NAME, Echo::Silent), LevelFilter::Debug);
    diag::milestone("boot: entry reached");
    if let Err(e) = uefi::helpers::init() {
        log::warn!("boot: uefi helpers init failed: {e}");
    }

    let status = system::with_stdout(|output| {
        if let Err(e) = output.reset(false) {
            log::warn!("console: reset failed: {e}");
        }
        system::with_stdin(|input| match App::new(input, output) {
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
    logger::with(|log| session::finish(log, EXIT_WAIT));
    status
}
