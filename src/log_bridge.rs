//! Bridges diagnostic tracing events into the [`RunEvent::Log`] stream.
//!
//! When NDJSON output is active (`--format json` or `--events <file>`), the
//! [`LogForwarder`] tracing layer captures each emitted event (level, target,
//! message, and remaining fields) and forwards it onto the run's event bus as a
//! [`RunEvent::Log`], so diagnostics reach the structured NDJSON surface in
//! addition to stderr.
//!
//! The layer sits in the global subscriber registry but stays inert until a run
//! installs an event sender via [`LogForwarder::install`]; headless and
//! `/dev/mem` NDJSON paths install it for the duration of the run and clear it
//! afterward. The TUI path leaves it inactive (it routes tracing to its own log
//! pane).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

use crate::events::{EventTx, RunEvent};

/// A tracing layer that forwards events onto an event bus as [`RunEvent::Log`].
///
/// Cloning shares the same install state, so the copy handed to the subscriber
/// registry and the copy retained by the run driver control one forwarder.
#[derive(Clone, Default)]
pub struct LogForwarder {
    /// Fast-path gate checked on every tracing event; avoids taking the mutex
    /// when no run has installed a sender.
    active: Arc<AtomicBool>,
    tx: Arc<Mutex<Option<EventTx>>>,
}

impl LogForwarder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Begin forwarding tracing events onto `tx` as [`RunEvent::Log`].
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn install(&self, tx: EventTx) {
        *self.tx.lock().expect("log forwarder mutex poisoned") = Some(tx);
        self.active.store(true, Ordering::Release);
    }

    /// Stop forwarding and drop the retained sender.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn clear(&self) {
        self.active.store(false, Ordering::Release);
        *self.tx.lock().expect("log forwarder mutex poisoned") = None;
    }
}

impl<S: tracing::Subscriber> Layer<S> for LogForwarder {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        // Clone the sender out and release the mutex before doing any work, so
        // the lock is never held across event formatting or the channel send.
        let tx = {
            let Ok(guard) = self.tx.lock() else { return };
            match guard.as_ref() {
                Some(tx) => tx.clone(),
                None => return,
            }
        };

        let mut visitor = LogVisitor::default();
        event.record(&mut visitor);
        let meta = event.metadata();

        // A full bus is not fatal for diagnostics; drop rather than block the
        // emitting thread.
        let message = std::mem::take(&mut visitor.message);
        let _ = tx.send(RunEvent::Log {
            level: *meta.level(),
            target: meta.target().to_owned(),
            message,
            fields: visitor.into_fields(),
        });
    }
}

/// Collects an event's `message` and remaining fields into strings.
///
/// `record_debug` and `record_str` are implemented; every other typed
/// `record_*` method defaults to `record_debug`, so most fields are captured
/// as their `Debug` rendering. This is a deliberately minimal projection --
/// structured typing is not preserved.
#[derive(Default)]
struct LogVisitor {
    message: String,
    fields: serde_json::Map<String, serde_json::Value>,
}

impl LogVisitor {
    fn into_fields(self) -> serde_json::Value {
        serde_json::Value::Object(self.fields)
    }
}

impl Visit for LogVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let rendered = format!("{value:?}");
        if field.name() == "message" {
            self.message = rendered;
        } else {
            self.fields
                .insert(field.name().to_owned(), serde_json::Value::String(rendered));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            value.clone_into(&mut self.message);
        } else {
            self.fields.insert(
                field.name().to_owned(),
                serde_json::Value::String(value.to_owned()),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};
    use tracing_subscriber::prelude::*;

    use super::*;

    /// Drain the bus and return the first `Log` event's (message, target, level).
    fn first_log(rx: &crate::events::EventRx) -> Option<(String, String, tracing::Level)> {
        rx.try_iter().find_map(|e| match e {
            RunEvent::Log {
                message,
                target,
                level,
                ..
            } => Some((message, target, level)),
            _ => None,
        })
    }

    #[test]
    fn forwards_message_level_and_fields_when_active() {
        let (tx, rx) = crate::events::event_bus();
        let fwd = LogForwarder::new();
        fwd.install(tx);

        let subscriber = tracing_subscriber::registry().with(fwd);
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(count = 3, "disk getting full");
        });

        let events: Vec<_> = rx.try_iter().collect();
        let log = events
            .iter()
            .find_map(|e| match e {
                RunEvent::Log {
                    message,
                    level,
                    fields,
                    ..
                } => Some((message, level, fields)),
                _ => None,
            })
            .expect("expected a Log event");
        check!(log.0 == "disk getting full");
        check!(*log.1 == tracing::Level::WARN);
        // Non-message fields land in `fields`, Debug-rendered.
        check!(log.2["count"] == "3");
    }

    #[test]
    fn inactive_forwarder_emits_nothing() {
        let (tx, rx) = crate::events::event_bus();
        let fwd = LogForwarder::new();
        // Not installed -> no sender, active=false.
        let subscriber = tracing_subscriber::registry().with(fwd);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("should be dropped");
        });
        drop(tx);
        assert!(first_log(&rx).is_none());
    }

    #[test]
    fn cleared_forwarder_stops_forwarding() {
        let (tx, rx) = crate::events::event_bus();
        let fwd = LogForwarder::new();
        fwd.install(tx);
        fwd.clear();

        let subscriber = tracing_subscriber::registry().with(fwd);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("after clear");
        });
        assert!(first_log(&rx).is_none());
    }

    #[test]
    fn target_is_captured() {
        let (tx, rx) = crate::events::event_bus();
        let fwd = LogForwarder::new();
        fwd.install(tx);
        let subscriber = tracing_subscriber::registry().with(fwd);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "ferrite::demo", "hello");
        });
        let (message, target, level) = first_log(&rx).expect("expected a Log event");
        check!(message == "hello");
        check!(target == "ferrite::demo");
        check!(level == tracing::Level::INFO);
    }

    #[test]
    fn str_valued_field_is_captured() {
        let (tx, rx) = crate::events::event_bus();
        let fwd = LogForwarder::new();
        fwd.install(tx);
        let subscriber = tracing_subscriber::registry().with(fwd);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(host = "roman", "connected");
        });
        let events: Vec<_> = rx.try_iter().collect();
        let fields = events
            .iter()
            .find_map(|e| match e {
                RunEvent::Log { fields, .. } => Some(fields.clone()),
                _ => None,
            })
            .expect("expected a Log event");
        check!(fields["host"] == "roman");
    }
}
