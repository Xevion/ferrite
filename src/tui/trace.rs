#![cfg_attr(coverage_nightly, coverage(off))]

use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use tracing_subscriber::fmt::MakeWriter;

use super::TuiEvent;

/// Shared routing state between [`TuiMakeWriter`] and [`TuiTraceGuard`].
///
/// While `active` is true, formatted trace output is sent to the TUI channel.
/// Once flipped to false (by dropping the guard), output routes to stderr.
pub struct TuiTraceState {
    active: AtomicBool,
}

impl TuiTraceState {
    #[must_use]
    pub(crate) const fn new() -> Self {
        Self {
            active: AtomicBool::new(true),
        }
    }

    fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
}

/// RAII guard that reroutes tracing from the TUI channel to stderr on drop.
///
/// When dropped:
/// 1. Flips the routing flag so future traces write to stderr.
/// 2. Drains any buffered `TuiEvent::Log` events from the channel to stderr.
pub struct TuiTraceGuard {
    state: Arc<TuiTraceState>,
    rx: mpsc::Receiver<TuiEvent>,
}

impl TuiTraceGuard {
    /// Wraps the receiver so buffered log events can be drained to stderr on drop.
    #[must_use]
    pub const fn new(state: Arc<TuiTraceState>, rx: mpsc::Receiver<TuiEvent>) -> Self {
        Self { state, rx }
    }
}

impl Drop for TuiTraceGuard {
    fn drop(&mut self) {
        self.state.active.store(false, Ordering::Release);

        while let Ok(event) = self.rx.try_recv() {
            if let TuiEvent::Log(msg) = event {
                let _ = io::stderr().write_all(msg.as_bytes());
                let _ = io::stderr().write_all(b"\n");
            }
        }
    }
}

/// A [`MakeWriter`] that routes formatted trace lines to the TUI channel
/// while the TUI is active, then to stderr after the [`TuiTraceGuard`] drops.
#[derive(Clone)]
pub struct TuiMakeWriter {
    tx: mpsc::SyncSender<TuiEvent>,
    state: Arc<TuiTraceState>,
}

impl TuiMakeWriter {
    /// Creates a writer factory sharing the given channel and routing state.
    #[must_use]
    pub const fn new(tx: mpsc::SyncSender<TuiEvent>, state: Arc<TuiTraceState>) -> Self {
        Self { tx, state }
    }
}

impl<'a> MakeWriter<'a> for TuiMakeWriter {
    type Writer = TuiWriter;

    fn make_writer(&'a self) -> Self::Writer {
        TuiWriter {
            tx: self.tx.clone(),
            state: Arc::clone(&self.state),
            buf: Vec::with_capacity(256),
        }
    }
}

/// Per-event writer that buffers a single formatted log line.
/// On drop, routes to the TUI channel or stderr based on [`TuiTraceState`].
pub struct TuiWriter {
    tx: mpsc::SyncSender<TuiEvent>,
    state: Arc<TuiTraceState>,
    buf: Vec<u8>,
}

impl Write for TuiWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for TuiWriter {
    fn drop(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let msg = String::from_utf8_lossy(&self.buf);
        let trimmed = msg.trim_end_matches('\n').to_string();
        if trimmed.is_empty() {
            return;
        }
        if self.state.is_active() {
            let _ = self.tx.try_send(TuiEvent::Log(trimmed));
        } else {
            let _ = io::stderr().write_all(trimmed.as_bytes());
            let _ = io::stderr().write_all(b"\n");
        }
    }
}

#[cfg(test)]
mod tests {
    use assert2::assert;

    use super::*;

    fn make_active_state() -> Arc<TuiTraceState> {
        Arc::new(TuiTraceState::new())
    }

    #[test]
    fn tui_writer_sends_through_channel() {
        let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
        let state = make_active_state();
        let mut writer = TuiWriter {
            tx,
            state,
            buf: Vec::new(),
        };
        writer.write_all(b" INFO ferrite: hello world\n").unwrap();
        drop(writer);
        match rx.try_recv() {
            Ok(TuiEvent::Log(msg)) => {
                assert!(msg.contains("hello world"));
                assert!(!msg.ends_with('\n'), "trailing newline should be trimmed");
            }
            other => panic!("expected TuiEvent::Log, got {other:?}"),
        }
    }

    #[test]
    fn tui_writer_empty_buffer_sends_nothing() {
        let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
        let state = make_active_state();
        let writer = TuiWriter {
            tx,
            state,
            buf: Vec::new(),
        };
        drop(writer);
        assert!(
            rx.try_recv().is_err(),
            "empty buffer should not send an event"
        );
    }

    #[test]
    fn tui_writer_routes_to_stderr_when_inactive() {
        let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
        let state = make_active_state();
        state.active.store(false, Ordering::Release);
        let mut writer = TuiWriter {
            tx,
            state,
            buf: Vec::new(),
        };
        writer.write_all(b"routed to stderr\n").unwrap();
        drop(writer);
        // Nothing sent to the TUI channel
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn tui_trace_guard_drains_and_deactivates() {
        let state = make_active_state();
        let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
        let _ = tx.try_send(TuiEvent::Log("buffered msg".into()));
        let _ = tx.try_send(TuiEvent::Tick); // non-Log events are discarded
        let _ = tx.try_send(TuiEvent::Log("second msg".into()));

        let guard = TuiTraceGuard::new(Arc::clone(&state), rx);
        assert!(state.is_active());
        drop(guard);
        assert!(!state.is_active());
        // rx is consumed by the guard — channel fully disconnected
    }
}
