//! Neutral pause signal shared between the interactive front-end and the
//! pattern-execution loop.
//!
//! The signal is deliberately front-end agnostic: the runner and pattern code
//! observe a plain [`AtomicBool`] and never depend on any TUI type. Headless
//! execution passes `None` (never paused); the TUI wires its segment's pause
//! flag in so the `p` key blocks the worker between work chunks.

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crate::shutdown;

/// A shared pause signal observed by the pattern loop. `None` (headless) is
/// never paused; `Some(flag)` is toggled by the front-end.
pub type PauseSignal<'a> = Option<&'a AtomicBool>;

/// How long the worker parks between re-checks while paused. Short enough to
/// feel responsive on resume/cancel, long enough to keep the parked thread off
/// the CPU.
const PARK_INTERVAL: Duration = Duration::from_millis(20);

/// Block the current thread while `pause` is set, returning once it clears.
///
/// Returns immediately when `pause` is `None` or already unset. Cancellation
/// always wins: a pending quit breaks out of the wait even if still paused, so
/// a paused run can never wedge shutdown.
#[inline]
pub fn wait_while_paused(pause: PauseSignal<'_>) {
    let Some(flag) = pause else { return };
    while flag.load(Ordering::Relaxed) && !shutdown::quit_requested() {
        thread::sleep(PARK_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use assert2::assert;
    use serial_test::serial;

    use super::*;

    #[test]
    fn none_returns_immediately() {
        // A missing signal must not block.
        wait_while_paused(None);
    }

    #[test]
    fn unset_flag_returns_immediately() {
        let flag = AtomicBool::new(false);
        wait_while_paused(Some(&flag));
    }

    #[test]
    #[serial]
    fn quit_breaks_out_of_paused_wait() {
        // Even while paused, a pending quit must let the wait return so
        // shutdown is never wedged by a paused worker.
        shutdown::reset();
        let flag = AtomicBool::new(true);
        shutdown::request_quit(shutdown::QuitReason::UserQuit);
        wait_while_paused(Some(&flag));
        shutdown::reset();
    }

    #[test]
    #[serial]
    fn resumes_when_flag_cleared_by_another_thread() {
        shutdown::reset();
        let flag = Arc::new(AtomicBool::new(true));
        let clearer = Arc::clone(&flag);
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(40));
            clearer.store(false, Ordering::Relaxed);
        });
        // Blocks until the spawned thread clears the flag.
        wait_while_paused(Some(&flag));
        assert!(!flag.load(Ordering::Relaxed));
        handle.join().unwrap();
        shutdown::reset();
    }
}
