//! Unattended-exit behaviour shared by the standalone apps.

use core::time::Duration;

use uefi::boot::{self, EventType, TimerTrigger, Tpl};
use uefi::runtime::{self, ResetType, VariableVendor};
use uefi::{Status, cstr16, guid, println, system};

use crate::bootlog::BootLog;

/// Show where the report went and wait up to `secs` for a key. With no key, a
/// run launched by systemd-boot cold-resets so the machine lands back in its
/// default OS; otherwise this returns and the caller exits to the firmware.
pub fn finish(report: &mut BootLog, secs: u64) {
    let saved = report.flush();
    println!(
        "\nreport file: {}\npress a key to exit (auto-exit in {secs}s)...",
        if saved { report.display_path() } else { "NOT written" }
    );
    if !wait_for_key_or_timeout(secs) && launched_by_systemd_boot() {
        runtime::reset(ResetType::COLD, Status::SUCCESS, None);
    }
}

/// True when systemd-boot chainloaded us: it publishes `LoaderInfo` before
/// starting an entry, and nothing else sets that variable.
#[must_use]
pub fn launched_by_systemd_boot() -> bool {
    let vendor = VariableVendor(guid!("4a67b082-0a4c-41cf-b6c7-440b29bb8c4f"));
    runtime::variable_exists(cstr16!("LoaderInfo"), &vendor).unwrap_or(false)
}

/// Wait for a keypress, giving up after `secs`. Returns whether a key was pressed.
pub fn wait_for_key_or_timeout(secs: u64) -> bool {
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
        },
    }
}
