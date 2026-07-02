#![cfg_attr(coverage_nightly, coverage(off))]

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;

use tracing::warn;

use crate::events::{EventRx, RunEvent};
use crate::ndjson::NdjsonEventWriter;

use super::{FlippedBits, Segment, TuiEvent, TuiFailure};

/// Bridges the runner's event bus to the TUI event channel.
///
/// Receives [`RunEvent`]s, updates [`Segment`] atomics (pattern index,
/// progress, failure count), and forwards translated [`TuiEvent`]s to the
/// TUI event loop.
pub struct EventBridge {
    segment: Arc<Segment>,
    tui_tx: mpsc::SyncSender<TuiEvent>,
    passes: usize,
    pattern_index: usize,
    done: bool,
}

impl EventBridge {
    /// Create a new bridge.
    ///
    /// `passes` is the total number of passes configured for the run,
    /// needed to detect when the segment is fully complete.
    #[must_use]
    pub fn new(segment: Arc<Segment>, tui_tx: mpsc::SyncSender<TuiEvent>, passes: usize) -> Self {
        Self {
            segment,
            tui_tx,
            passes,
            pattern_index: 0,
            done: false,
        }
    }

    /// Process a single [`RunEvent`].
    ///
    /// Updates segment state and forwards TUI events as needed.
    /// Returns `true` if the bridge should continue processing, `false`
    /// if the run is complete and the loop should exit.
    pub fn handle_event(&mut self, event: &RunEvent) -> bool {
        match event {
            RunEvent::TestStart { .. } => {
                self.segment.set_pattern(self.pattern_index);
            }
            RunEvent::Progress {
                sub_pass, total, ..
            } => {
                let bp = if *total > 0 {
                    (u128::from(*sub_pass) * 10000 / u128::from(*total)) as u64
                } else {
                    0
                };
                self.segment.progress_bp.store(bp, Ordering::Relaxed);
            }
            RunEvent::TestComplete {
                pattern, failures, ..
            } => {
                self.segment.progress_bp.store(10000, Ordering::Relaxed);
                self.pattern_index += 1;

                for f in failures {
                    self.segment.record_failure();
                    if let Err(e) = self.tui_tx.try_send(TuiEvent::Failure(TuiFailure {
                        segment_name: self.segment.name.clone(),
                        address: f.addr as u64,
                        expected: f.expected,
                        actual: f.actual,
                        flipped_bits: FlippedBits::from_mismatch(f.expected, f.actual),
                        pattern: pattern.to_string(),
                        progress_fraction: 1.0,
                    })) {
                        warn!("TUI channel full, dropped failure event: {e}");
                    }
                }
            }
            RunEvent::PassComplete { pass, .. } => {
                self.pattern_index = 0;
                if *pass >= self.passes {
                    self.done = true;
                    if let Err(e) = self.tui_tx.try_send(TuiEvent::Done) {
                        warn!("TUI channel full, dropped Done event: {e}");
                    }
                }
            }
            RunEvent::EccDeltas { deltas, .. } => {
                for d in deltas {
                    warn!(
                        mc = d.mc,
                        dimm = d.dimm_index,
                        ce = d.ce_delta,
                        ue = d.ue_delta,
                        "ECC event detected"
                    );
                }
            }
            RunEvent::RunComplete => return false,
            _ => {}
        }

        true
    }

    /// Run the bridge loop, consuming events until [`RunEvent::RunComplete`]
    /// or channel disconnect.
    ///
    /// If an [`NdjsonEventWriter`] is provided, each event is also written
    /// to the NDJSON stream (for `--events <file>` support).
    ///
    /// After exiting, sends [`TuiEvent::Done`] if the segment didn't
    /// complete naturally (e.g. user quit early).
    pub fn run(
        mut self,
        event_rx: &EventRx,
        mut ndjson: Option<NdjsonEventWriter>,
    ) -> Option<NdjsonEventWriter> {
        while let Ok(event) = event_rx.recv() {
            if let Some(w) = ndjson.as_mut() {
                w.handle_event(&event);
            }
            if !self.handle_event(&event) {
                break;
            }
        }

        if !self.done
            && let Err(e) = self.tui_tx.try_send(TuiEvent::Done)
        {
            warn!("TUI channel full, dropped cleanup Done event: {e}");
        }

        ndjson
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;
    use std::time::Duration;

    use assert2::{assert, check};

    use crate::events::RunEvent;
    use crate::pattern::Pattern;
    use crate::tui::{Segment, TuiEvent};

    use super::EventBridge;

    fn make_segment(patterns: &[&str]) -> Arc<Segment> {
        let names: Vec<String> = patterns.iter().map(|s| (*s).to_string()).collect();
        Arc::new(Segment::new("r0".into(), 8 * 1024 * 1024, names))
    }

    fn make_bridge(
        segment: Arc<Segment>,
        passes: usize,
    ) -> (EventBridge, mpsc::Receiver<TuiEvent>) {
        let (tui_tx, tui_rx) = mpsc::sync_channel::<TuiEvent>(256);
        let bridge = EventBridge::new(segment, tui_tx, passes);
        (bridge, tui_rx)
    }

    #[test]
    fn test_start_sets_pattern() {
        let segment = make_segment(&["solid", "walk"]);
        let (mut bridge, _rx) = make_bridge(Arc::clone(&segment), 1);

        bridge.handle_event(&RunEvent::TestStart {
            pattern: Pattern::SolidBits,
            pass: 1,
        });

        check!(segment.current_pattern() == "solid");
    }

    #[test]
    fn progress_updates_segment() {
        let segment = make_segment(&["solid"]);
        let (mut bridge, _rx) = make_bridge(Arc::clone(&segment), 1);

        bridge.handle_event(&RunEvent::Progress {
            pattern: Pattern::SolidBits,
            pass: 1,
            sub_pass: 50,
            total: 100,
        });

        check!(segment.progress_bp.load(Ordering::Relaxed) == 5000);
    }

    #[test]
    fn progress_zero_total_stores_zero() {
        let segment = make_segment(&["solid"]);
        let (mut bridge, _rx) = make_bridge(Arc::clone(&segment), 1);

        bridge.handle_event(&RunEvent::Progress {
            pattern: Pattern::SolidBits,
            pass: 1,
            sub_pass: 50,
            total: 0,
        });

        check!(segment.progress_bp.load(Ordering::Relaxed) == 0);
    }

    #[test]
    fn test_complete_advances_pattern_index() {
        let segment = make_segment(&["solid", "walk"]);
        let (mut bridge, _rx) = make_bridge(Arc::clone(&segment), 1);

        // Start first pattern
        bridge.handle_event(&RunEvent::TestStart {
            pattern: Pattern::SolidBits,
            pass: 1,
        });
        check!(segment.current_pattern() == "solid");

        // Complete it
        bridge.handle_event(&RunEvent::TestComplete {
            pattern: Pattern::SolidBits,
            pass: 1,
            elapsed: Duration::from_millis(100),
            bytes: 8192,
            failures: vec![],
            interrupted: false,
        });

        // Next test start should set pattern index 1
        bridge.handle_event(&RunEvent::TestStart {
            pattern: Pattern::WalkingOnes,
            pass: 1,
        });
        check!(segment.current_pattern() == "walk");
    }

    #[test]
    fn test_complete_with_failures_sends_errors() {
        use crate::failure::FailureBuilder;

        let segment = make_segment(&["solid"]);
        let (mut bridge, rx) = make_bridge(Arc::clone(&segment), 1);

        let failure = FailureBuilder::default()
            .addr(0xdead_0000_usize)
            .expected(0xFF_u64)
            .actual(0xFE_u64)
            .build();

        bridge.handle_event(&RunEvent::TestComplete {
            pattern: Pattern::SolidBits,
            pass: 1,
            elapsed: Duration::from_millis(100),
            bytes: 8192,
            failures: vec![failure],
            interrupted: false,
        });

        check!(segment.failure_count.load(Ordering::Relaxed) == 1);

        match rx.try_recv() {
            Ok(TuiEvent::Failure(f)) => {
                check!(f.segment_name == "r0");
                check!(f.expected == 0xFF);
                check!(f.actual == 0xFE);
                check!(f.flipped_bits == crate::tui::FlippedBits::Single(0));
            }
            other => panic!("expected TuiEvent::Failure, got {other:?}"),
        }
    }

    #[test]
    fn pass_complete_resets_pattern_index() {
        let segment = make_segment(&["solid", "walk"]);
        let (mut bridge, _rx) = make_bridge(Arc::clone(&segment), 2);

        // Simulate completing first pass
        bridge.handle_event(&RunEvent::TestComplete {
            pattern: Pattern::SolidBits,
            pass: 1,
            elapsed: Duration::from_millis(50),
            bytes: 4096,
            failures: vec![],
            interrupted: false,
        });
        // pattern_index is now 1

        bridge.handle_event(&RunEvent::PassComplete {
            pass: 1,
            failures: 0,
            elapsed: Duration::from_millis(100),
        });

        // After pass complete, next TestStart should use index 0 again
        bridge.handle_event(&RunEvent::TestStart {
            pattern: Pattern::SolidBits,
            pass: 2,
        });
        check!(segment.current_pattern() == "solid");
    }

    #[test]
    fn pass_complete_final_pass_sends_done() {
        let segment = make_segment(&["solid"]);
        let (mut bridge, rx) = make_bridge(segment, 1);

        bridge.handle_event(&RunEvent::PassComplete {
            pass: 1,
            failures: 0,
            elapsed: Duration::from_millis(100),
        });

        match rx.try_recv() {
            Ok(TuiEvent::Done) => {}
            other => panic!("expected TuiEvent::Done, got {other:?}"),
        }
    }

    #[test]
    fn pass_complete_non_final_no_done() {
        let segment = make_segment(&["solid"]);
        let (mut bridge, rx) = make_bridge(segment, 3);

        bridge.handle_event(&RunEvent::PassComplete {
            pass: 1,
            failures: 0,
            elapsed: Duration::from_millis(100),
        });

        check!(rx.try_recv().is_err());
    }

    #[test]
    fn run_complete_returns_false() {
        let segment = make_segment(&["solid"]);
        let (mut bridge, _rx) = make_bridge(segment, 1);

        let cont = bridge.handle_event(&RunEvent::RunComplete);
        assert!(!cont);
    }

    #[test]
    fn run_sends_done_for_incomplete_segment() {
        let segment = make_segment(&["solid"]);
        let (bridge, rx) = make_bridge(segment, 1);

        let (event_tx, event_rx) = crate::events::event_bus();
        // Send RunComplete without any PassComplete -- the segment never completed.
        event_tx.send(RunEvent::RunComplete).unwrap();
        drop(event_tx);

        bridge.run(&event_rx, None);

        let done_count = std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|e| matches!(e, TuiEvent::Done))
            .count();
        check!(done_count == 1);
    }

    #[test]
    fn run_no_duplicate_done_for_completed_segment() {
        let segment = make_segment(&["solid"]);
        let (bridge, rx) = make_bridge(segment, 1);

        let (event_tx, event_rx) = crate::events::event_bus();
        // Segment completes naturally, then RunComplete
        event_tx
            .send(RunEvent::PassComplete {
                pass: 1,
                failures: 0,
                elapsed: Duration::from_millis(100),
            })
            .unwrap();
        event_tx.send(RunEvent::RunComplete).unwrap();
        drop(event_tx);

        bridge.run(&event_rx, None);

        let done_count = std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|e| matches!(e, TuiEvent::Done))
            .count();
        // One from PassComplete, zero from cleanup
        check!(done_count == 1);
    }

    #[test]
    fn ecc_deltas_handled_without_panic() {
        let segment = make_segment(&["solid"]);
        let (mut bridge, _rx) = make_bridge(segment, 1);

        let cont = bridge.handle_event(&RunEvent::EccDeltas {
            pass: 1,
            deltas: vec![crate::edac::EccDelta {
                mc: 0,
                dimm_index: 1,
                label: Some("DIMM_A1".to_owned()),
                ce_delta: 2,
                ue_delta: 0,
            }],
        });
        assert!(cont);
    }

    #[test]
    fn run_on_disconnect_sends_done_for_incomplete() {
        let segment = make_segment(&["solid"]);
        let (bridge, rx) = make_bridge(segment, 1);

        let (event_tx, event_rx) = crate::events::event_bus();
        // Drop sender without sending RunComplete -- simulates disconnect
        drop(event_tx);

        bridge.run(&event_rx, None);

        let done_count = std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|e| matches!(e, TuiEvent::Done))
            .count();
        check!(done_count == 1);
    }
}
