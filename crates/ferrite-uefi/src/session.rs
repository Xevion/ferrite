//! Report output and unattended-exit behaviour shared by the standalone apps.

use alloc::format;
use alloc::string::String;
use core::time::Duration;

use uefi::boot::{self, EventType, TimerTrigger, Tpl};
use uefi::fs::{FileSystem, PathBuf};
use uefi::runtime::{self, ResetType, VariableVendor};
use uefi::{CString16, Status, cstr16, guid, println, system};

/// Lines mirrored to the console and to `\EFI\ferrite\<name>.txt` on the boot
/// volume. The file is rewritten on every line so a hang still leaves it behind.
pub struct Report {
    fs: Option<FileSystem>,
    path: Option<PathBuf>,
    display: String,
    text: String,
}

impl Report {
    #[must_use]
    pub fn open(name: &str) -> Self {
        let display = format!("\\EFI\\ferrite\\{name}.txt");
        let mut fs = boot::get_image_file_system(boot::image_handle())
            .ok()
            .map(FileSystem::new);
        if let Some(fs) = fs.as_mut() {
            let _ = fs.create_dir_all(cstr16!("\\EFI\\ferrite"));
        }
        Self {
            fs,
            path: CString16::try_from(display.as_str())
                .ok()
                .map(PathBuf::from),
            display,
            text: String::new(),
        }
    }

    pub fn line(&mut self, s: &str) {
        self.text.push_str(s);
        self.text.push('\n');
        println!("{s}");
        self.flush();
    }

    /// Rewrite the report file; a failure only loses the file copy.
    pub fn flush(&mut self) -> bool {
        match (self.fs.as_mut(), self.path.as_ref()) {
            (Some(fs), Some(path)) => fs.write(path, self.text.as_bytes()).is_ok(),
            _ => false,
        }
    }
}

/// Show where the report went and wait up to `secs` for a key. With no key, a
/// run launched by systemd-boot cold-resets so the machine lands back in its
/// default OS; otherwise this returns and the caller exits to the firmware.
pub fn finish(report: &mut Report, secs: u64) {
    let saved = report.flush();
    println!(
        "\nreport file: {}\npress a key to exit (auto-exit in {secs}s)...",
        if saved {
            report.display.as_str()
        } else {
            "NOT written"
        }
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
        }
    }
}
