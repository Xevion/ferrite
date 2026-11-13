//! Unattended-exit behaviour shared by the standalone apps.

use core::time::Duration;

use uefi::boot::{self, EventType, TimerTrigger, Tpl};
use uefi::runtime::{self, ResetType, VariableVendor};
use uefi::{Status, cstr16, guid, system};

use crate::bootlog::BootLog;
use crate::{clock, link, logger};

/// A systemd-boot run is normally driven remotely, so nobody is waiting to read the screen.
const UNATTENDED_WAIT: Duration = Duration::from_secs(3);

/// Show where the report went and wait up to `wait` for a key (at most 3 s when unattended).
///
/// With no key, a run launched by systemd-boot cold-resets so the machine lands
/// back in its default OS; otherwise this returns and the caller exits to the firmware.
/// Under the loader (its link protocol is installed) it saves the report and returns at
/// once, so the loader gets control back.
#[expect(clippy::disallowed_methods, reason = "session is the one home of the reset")]
pub fn finish(report: &mut BootLog, wait: Duration) {
    if link::is_present() {
        let path = if report.flush() { report.display_path() } else { "NOT written" };
        log::info!("report file: {path}");
        return;
    }
    let unattended = launched_by_systemd_boot();
    let wait = if unattended { wait.min(UNATTENDED_WAIT) } else { wait };
    let saved = report.flush();
    let path = if saved { report.display_path() } else { "NOT written" };
    let note = alloc::format!("\nreport file: {path}\npress a key to exit (auto-exit in {}s)...", wait.as_secs());
    report.line(&note);
    if !wait_for_key_or_timeout(wait) && unattended {
        runtime::reset(ResetType::COLD, Status::SUCCESS, None);
    }
}

/// Cold-reset now, after saving the log: the loader's answer to a reboot request.
///
/// The machine lands in the default OS, because the one-shot boot entry that started the loader
/// was spent when it started.
#[expect(clippy::disallowed_methods, reason = "session is the one home of the reset")]
pub fn reboot() -> ! {
    let _ = logger::with(BootLog::flush);
    runtime::reset(ResetType::COLD, Status::SUCCESS, None)
}

/// True when systemd-boot chainloaded us: it publishes `LoaderInfo` before
/// starting an entry, and nothing else sets that variable.
#[must_use]
pub fn launched_by_systemd_boot() -> bool {
    let vendor = VariableVendor(guid!("4a67b082-0a4c-41cf-b6c7-440b29bb8c4f"));
    runtime::variable_exists(cstr16!("LoaderInfo"), &vendor).unwrap_or(false)
}

/// Wait for a keypress, giving up after `timeout`; false only when it all elapsed.
///
/// A key or a firmware error while waiting returns true, so a failure never
/// triggers the unattended reset. Without a timer event it stalls for the whole timeout.
pub fn wait_for_key_or_timeout(timeout: Duration) -> bool {
    // SAFETY: no notify function is registered, so no callback can run after the event is closed or boot services exit.
    let timer = unsafe { boot::create_event(EventType::TIMER, Tpl::APPLICATION, None, None) };
    let Ok(timer) = timer else {
        clock::stall(timeout);
        return false;
    };
    if boot::set_timer(&timer, TimerTrigger::Relative(timeout)).is_err() {
        close_timer(timer);
        clock::stall(timeout);
        return false;
    }
    let (interrupted, timer) = if let Some(key) = system::with_stdin(|input| input.wait_for_key_event().ok()) {
        let mut events = [key, timer];
        let woken = boot::wait_for_event(&mut events);
        let [_, timer] = events;
        (!matches!(woken, Ok(1)), timer)
    } else {
        let mut events = [timer];
        let woken = boot::wait_for_event(&mut events);
        let [timer] = events;
        (woken.is_err(), timer)
    };
    close_timer(timer);
    interrupted
}

fn close_timer(timer: uefi::Event) {
    if let Err(e) = boot::close_event(timer) {
        log::debug!("closing the exit timer failed: {e}");
    }
}

/// Records the panic in the boot log, shows it, then after ten seconds resets (under systemd-boot) or powers off.
/// On an AP it only halts that core, which the firmware's dispatch timeout then resets.
#[cfg(all(ferrite_app, feature = "panic-handler"))]
#[panic_handler]
#[expect(clippy::disallowed_macros, reason = "the panic handler is one of the two sanctioned println homes")]
#[expect(clippy::disallowed_methods, reason = "session is the one home of the reset, here for the panic path")]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    use core::sync::atomic::{AtomicBool, Ordering};

    static PANICKING: AtomicBool = AtomicBool::new(false);

    // An AP may not touch the logger, the console or any EFI service, so it only halts.
    if crate::mp::on_ap() {
        crate::mp::halt_ap();
    }
    if !PANICKING.swap(true, Ordering::Relaxed) {
        if !logger::record_panic(info) {
            uefi::println!("[PANIC]: {info}");
        }
        if boot_services_active() {
            clock::stall(Duration::from_secs(10));
        }
        // Under systemd-boot the one-shot entry is spent, so a cold reset lands back in the default OS.
        let kind = if launched_by_systemd_boot() { ResetType::COLD } else { ResetType::SHUTDOWN };
        runtime::reset(kind, Status::ABORTED, None);
    }
    loop {
        core::hint::spin_loop();
    }
}

#[cfg(all(ferrite_app, feature = "panic-handler"))]
fn boot_services_active() -> bool {
    let Some(table) = uefi::table::system_table_raw() else {
        return false;
    };
    // SAFETY: the pointer is valid once the firmware has handed the system table to the crate.
    !unsafe { table.as_ref() }.boot_services.is_null()
}
