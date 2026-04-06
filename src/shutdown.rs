//! Unified shutdown infrastructure: signal handling, quit flag, escalation, terminal cleanup.

use std::process;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::thread::{self, JoinHandle};

use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

static QUIT: AtomicBool = AtomicBool::new(false);
static SIGCOUNT: AtomicU8 = AtomicU8::new(0);
static QUIT_REASON: AtomicU8 = AtomicU8::new(0);

/// Why the program is shutting down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum QuitReason {
    /// No quit requested (still running or completed normally).
    None = 0,
    /// User deliberately quit via 'q', Esc, or normal completion.
    UserQuit = 1,
    /// External signal (SIGINT/SIGTERM).
    Signal = 2,
}

impl From<u8> for QuitReason {
    fn from(v: u8) -> Self {
        match v {
            1 => Self::UserQuit,
            2 => Self::Signal,
            _ => Self::None,
        }
    }
}

/// Check whether a quit has been requested.
#[inline]
pub fn quit_requested() -> bool {
    QUIT.load(Ordering::Acquire)
}

/// Request a graceful quit with the given reason.
///
/// The reason is first-write-wins: if a reason is already set, subsequent
/// calls with a different reason are ignored. This ensures the original
/// cause of shutdown is preserved.
pub fn request_quit(reason: QuitReason) {
    // First-write-wins: only set reason if it's currently None.
    let _ = QUIT_REASON.compare_exchange(
        QuitReason::None as u8,
        reason as u8,
        Ordering::Relaxed,
        Ordering::Relaxed,
    );
    // Release ensures the QUIT_REASON write above is visible to any thread
    // that observes QUIT=true via an Acquire load.
    QUIT.store(true, Ordering::Release);
}

/// Escalate shutdown: increment the signal counter and request quit.
///
/// On the first call, sets the quit flag with [`QuitReason::Signal`].
/// On the second call (or later), force-exits the process immediately
/// with terminal cleanup.
pub fn escalate() {
    let prev = SIGCOUNT.fetch_add(1, Ordering::Relaxed);
    request_quit(QuitReason::Signal);
    if prev >= 1 {
        force_exit(130);
    }
}

/// Read the reason for shutdown.
pub fn quit_reason() -> QuitReason {
    QuitReason::from(QUIT_REASON.load(Ordering::Acquire))
}

/// Compute the appropriate exit code based on quit reason and error count.
#[must_use]
pub fn exit_code(total_failures: usize) -> i32 {
    if total_failures > 0 {
        return 1;
    }
    match quit_reason() {
        QuitReason::Signal => 130,
        QuitReason::UserQuit | QuitReason::None => 0,
    }
}

/// Force-exit the process with terminal cleanup.
///
/// Attempts to restore the terminal (disable raw mode) before calling
/// [`process::exit`]. This is the "nuclear option" for when graceful shutdown
/// has been requested but the program isn't responding.
pub fn force_exit(code: i32) -> ! {
    #[cfg(feature = "tui")]
    {
        let _ = crossterm::terminal::disable_raw_mode();
    }
    process::exit(code);
}

/// Handle returned by [`install_signal_handlers`]. Owns the signal-watcher
/// thread and the iterator handle needed to shut it down.
pub struct ShutdownHandle {
    signal_handle: signal_hook::iterator::Handle,
    thread: JoinHandle<()>,
}

impl ShutdownHandle {
    /// Stop the signal-watcher thread and wait for it to exit.
    pub fn shutdown(self) {
        self.signal_handle.close();
        if let Err(payload) = self.thread.join() {
            std::panic::resume_unwind(payload);
        }
    }
}

/// Register SIGINT and SIGTERM handlers and spawn a signal-watcher thread.
///
/// The watcher thread calls [`escalate`] for each received signal:
/// - First signal: sets quit flag
/// - Second signal: force-exits with terminal cleanup
///
/// # Errors
///
/// Returns an error if signal registration fails.
pub fn install_signal_handlers() -> anyhow::Result<ShutdownHandle> {
    let mut signals = Signals::new([SIGINT, SIGTERM])?;
    let handle = signals.handle();

    let thread = thread::Builder::new()
        .name("signal-watcher".into())
        .spawn(move || {
            for _sig in signals.forever() {
                escalate();
            }
        })?;

    Ok(ShutdownHandle {
        signal_handle: handle,
        thread,
    })
}

/// Install a panic hook that restores the terminal before printing the panic.
///
/// This is belt-and-suspenders alongside [`TerminalGuard`]: the guard handles
/// normal exits and unwind-style panics, while this hook fires even with
/// `panic = "abort"` (where Drop doesn't run).
///
/// Safe to call even when the TUI isn't active -- `disable_raw_mode` is
/// idempotent.
pub fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        #[cfg(feature = "tui")]
        {
            let _ = crossterm::terminal::disable_raw_mode();
        }
        original(info);
    }));
}

/// RAII guard that enables raw mode on creation and disables it on drop
/// (including panic unwind).
#[cfg(feature = "tui")]
pub struct TerminalGuard;

#[cfg(feature = "tui")]
impl TerminalGuard {
    /// Enable raw mode.
    ///
    /// # Errors
    ///
    /// Returns an error if raw mode cannot be enabled.
    pub fn new() -> anyhow::Result<Self> {
        crossterm::terminal::enable_raw_mode().map_err(|e| {
            anyhow::anyhow!("failed to enable raw mode (is stdout a terminal?): {e}")
        })?;
        Ok(Self)
    }
}

#[cfg(feature = "tui")]
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

#[cfg(test)]
pub fn reset() {
    QUIT.store(false, Ordering::Relaxed);
    SIGCOUNT.store(0, Ordering::Relaxed);
    QUIT_REASON.store(QuitReason::None as u8, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};
    use serial_test::serial;

    use super::*;

    // Tests in this module mutate process-global atomics (QUIT, SIGCOUNT,
    // QUIT_REASON). Under `cargo nextest` each test gets its own process, so
    // this is safe. The `#[serial]` attribute ensures safety under `cargo test`
    // as well (which runs tests as threads in a single process).

    #[test]
    #[serial]
    fn quit_requested_default_false() {
        reset();
        assert!(!quit_requested());
    }

    #[test]
    #[serial]
    fn request_quit_sets_flag_and_reason() {
        reset();
        request_quit(QuitReason::UserQuit);
        assert!(quit_requested());
        check!(quit_reason() == QuitReason::UserQuit);
    }

    #[test]
    #[serial]
    fn quit_reason_first_write_wins() {
        reset();
        request_quit(QuitReason::UserQuit);
        request_quit(QuitReason::Signal);
        check!(quit_reason() == QuitReason::UserQuit);
    }

    #[test]
    #[serial]
    fn escalate_first_call_sets_quit() {
        reset();
        // First escalate should set quit but not exit
        escalate();
        assert!(quit_requested());
        check!(quit_reason() == QuitReason::Signal);
        check!(SIGCOUNT.load(Ordering::Relaxed) == 1);
    }

    #[test]
    #[serial]
    fn exit_code_with_errors() {
        reset();
        check!(exit_code(5) == 1);
    }

    #[test]
    #[serial]
    fn exit_code_signal_no_errors() {
        reset();
        request_quit(QuitReason::Signal);
        check!(exit_code(0) == 130);
    }

    #[test]
    #[serial]
    fn exit_code_user_quit_no_errors() {
        reset();
        request_quit(QuitReason::UserQuit);
        check!(exit_code(0) == 0);
    }

    #[test]
    #[serial]
    fn exit_code_normal_completion() {
        reset();
        check!(exit_code(0) == 0);
    }

    #[test]
    fn quit_reason_from_u8_roundtrips() {
        check!(QuitReason::from(0) == QuitReason::None);
        check!(QuitReason::from(1) == QuitReason::UserQuit);
        check!(QuitReason::from(2) == QuitReason::Signal);
        check!(QuitReason::from(255) == QuitReason::None);
    }

    #[test]
    #[serial]
    fn install_and_shutdown_signal_handlers() {
        reset();
        let handle = install_signal_handlers().expect("signal handler install failed");
        handle.shutdown();
    }
}
