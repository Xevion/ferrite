//! Headless diagnostics written to the FAT volume we booted from.
//!
//! A target box with no screen gives us no feedback, so key milestones are
//! mirrored to `\ferrite.log` on the boot USB: power off, pull the stick, read
//! the file. The whole document is rewritten on every flush
//! ([`FileSystem::write`] truncates), so whatever we captured survives the
//! abrupt power-off that stops a headless machine.
//!
//! The *absence* of the file is itself a signal: if the log never appears, the
//! app never ran -- a firmware boot-policy problem, not a code problem.

use alloc::string::String;
use core::fmt::Write as _;

use uefi::boot;
use uefi::fs::FileSystem;
use uefi::{CStr16, cstr16};

/// Log lives at the volume root so it's trivial to find on the pulled USB.
const LOG_PATH: &CStr16 = cstr16!("\\ferrite.log");
/// Liveness cadence: emit an `alive` line at least this often so a healthy but
/// quiet box still proves its loop is turning.
const KEEPALIVE_MS: u64 = 30_000;

pub struct Diag {
    fs: Option<FileSystem>,
    log: String,
    last_net: String,
    last_keepalive: u64,
}

impl Diag {
    /// Open the boot volume's file system and start a fresh log. If the volume
    /// can't be opened, logging degrades to a no-op and the app runs unchanged.
    #[must_use]
    pub fn open() -> Self {
        let fs = boot::get_image_file_system(boot::image_handle())
            .ok()
            .map(FileSystem::new);
        let mut diag = Self {
            fs,
            log: String::new(),
            last_net: String::new(),
            last_keepalive: 0,
        };
        diag.event(0, "boot: entry reached, uefi init ok");
        diag
    }

    /// Append a timestamped milestone line and flush immediately.
    pub fn event(&mut self, ms: u64, msg: &str) {
        let _ = writeln!(self.log, "[{ms:>8}] {msg}");
        self.flush();
    }

    /// Per-tick update: log the network snapshot only when it changes, plus a
    /// periodic `alive` line so a healthy-but-idle box still shows progress.
    /// `sweep` is a short one-line summary of the current sweep state.
    pub fn tick(&mut self, ms: u64, net: &str, sweep: &str) {
        if net != self.last_net {
            self.last_net.clear();
            self.last_net.push_str(net);
            let _ = writeln!(self.log, "[{ms:>8}] {net}");
            self.flush();
        }
        if ms.saturating_sub(self.last_keepalive) >= KEEPALIVE_MS {
            self.last_keepalive = ms;
            let _ = writeln!(self.log, "[{ms:>8}] alive {sweep}");
            self.flush();
        }
    }

    /// Rewrite the whole log file. A failed write just loses logging; the tester
    /// keeps running, so errors are deliberately swallowed.
    fn flush(&mut self) {
        if let Some(fs) = self.fs.as_mut() {
            let _ = fs.write(LOG_PATH, self.log.as_bytes());
        }
    }
}
