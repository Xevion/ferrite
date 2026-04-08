#![cfg_attr(coverage_nightly, coverage(off))]

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;

use tracing::warn;

use crate::events::{EventRx, RegionEvent, RunEvent};

use super::{FlippedBits, Segment, TuiEvent, TuiFailure};

/// Bridges the runner's event bus to the TUI event channel.
///
/// Receives [`RunEvent`]s, updates [`Segment`] atomics (pattern index,
/// progress, failure count), and forwards translated [`TuiEvent`]s to the
/// TUI event loop.
pub struct EventBridge {
    regions: Vec<Arc<Segment>>,
    tui_tx: mpsc::SyncSender<TuiEvent>,
    passes: usize,
    pattern_indices: Vec<usize>,
    regions_done: Vec<bool>,
}

impl EventBridge {
    /// Create a new bridge.
    ///
    /// `passes` is the total number of passes configured for the run —
    /// used to detect when a region is fully complete.
    #[must_use]
    pub fn new(
        regions: Vec<Arc<Segment>>,
        tui_tx: mpsc::SyncSender<TuiEvent>,
        passes: usize,
    ) -> Self {
        let n = regions.len();
        Self {
            regions,
            tui_tx,
            passes,
            pattern_indices: vec![0usize; n],
            regions_done: vec![false; n],
        }
    }

    /// Process a single [`RunEvent`].
    ///
    /// Updates segment state and forwards TUI events as needed.
    /// Returns `true` if the bridge should continue processing, `false`
    /// if the run is complete and the loop should exit.
    pub fn handle_event(&mut self, event: &RunEvent) -> bool {
        let n = self.regions.len();

        match event {
            RunEvent::Region(idx, region_event) if *idx < n => {
                let idx = *idx;
                let segment = &self.regions[idx];

                match region_event {
                    RegionEvent::TestStart { .. } => {
                        segment.set_pattern(self.pattern_indices[idx]);
                    }
                    RegionEvent::Progress {
                        sub_pass, total, ..
                    } => {
                        let bp = if *total > 0 {
                            (u128::from(*sub_pass) * 10000 / u128::from(*total)) as u64
                        } else {
                            0
                        };
                        segment.progress_bp.store(bp, Ordering::Relaxed);
                    }
                    RegionEvent::TestComplete {
                        pattern, failures, ..
                    } => {
                        segment.progress_bp.store(10000, Ordering::Relaxed);
                        self.pattern_indices[idx] += 1;

                        for f in failures {
                            segment.record_failure();
                            if let Err(e) = self.tui_tx.try_send(TuiEvent::Failure(TuiFailure {
                                region_idx: idx,
                                region_name: segment.name.clone(),
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
                    RegionEvent::PassComplete { pass, .. } => {
                        self.pattern_indices[idx] = 0;
                        if *pass >= self.passes {
                            self.regions_done[idx] = true;
                            if let Err(e) = self.tui_tx.try_send(TuiEvent::RegionDone(idx)) {
                                warn!(region = idx, "TUI channel full, dropped RegionDone: {e}");
                            }
                        }
                    }
                    RegionEvent::EccDeltas { deltas, .. } => {
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
                    RegionEvent::PassStart { .. } => {}
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
    /// After exiting, sends [`TuiEvent::RegionDone`] for any regions that
    /// didn't complete naturally (e.g. user quit early).
    pub fn run(mut self, event_rx: &EventRx) {
        while let Ok(event) = event_rx.recv() {
            if !self.handle_event(&event) {
                break;
            }
        }

        // Signal done for any regions that didn't complete naturally.
        for (i, done) in self.regions_done.iter().enumerate() {
            if !done && let Err(e) = self.tui_tx.try_send(TuiEvent::RegionDone(i)) {
                warn!(
                    region = i,
                    "TUI channel full, dropped cleanup RegionDone: {e}"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;
    use std::time::Duration;

    use assert2::{assert, check};

    use crate::events::{RegionEvent, RunEvent};
    use crate::pattern::Pattern;
    use crate::tui::{Segment, TuiEvent};

    use super::EventBridge;

    fn make_regions(n: usize, patterns: &[&str]) -> Vec<Arc<Segment>> {
        let names: Vec<String> = patterns.iter().map(|s| (*s).to_string()).collect();
        (0..n)
            .map(|i| {
                Arc::new(Segment::new(
                    format!("r{i}"),
                    8 * 1024 * 1024,
                    names.clone(),
                ))
            })
            .collect()
    }

    fn make_bridge(
        regions: Vec<Arc<Segment>>,
        passes: usize,
    ) -> (EventBridge, mpsc::Receiver<TuiEvent>) {
        let (tui_tx, tui_rx) = mpsc::sync_channel::<TuiEvent>(256);
        let bridge = EventBridge::new(regions, tui_tx, passes);
        (bridge, tui_rx)
    }

    #[test]
    fn test_start_sets_pattern() {
        let regions = make_regions(1, &["solid", "walk"]);
        let (mut bridge, _rx) = make_bridge(regions.clone(), 1);

        bridge.handle_event(&RunEvent::Region(
            0,
            RegionEvent::TestStart {
                pattern: Pattern::SolidBits,
                pass: 1,
            },
        ));

        check!(regions[0].current_pattern() == "solid");
    }

    #[test]
    fn progress_updates_segment() {
        let regions = make_regions(1, &["solid"]);
        let (mut bridge, _rx) = make_bridge(regions.clone(), 1);

        bridge.handle_event(&RunEvent::Region(
            0,
            RegionEvent::Progress {
                pattern: Pattern::SolidBits,
                pass: 1,
                sub_pass: 50,
                total: 100,
            },
        ));

        check!(regions[0].progress_bp.load(Ordering::Relaxed) == 5000);
    }

    #[test]
    fn progress_zero_total_stores_zero() {
        let regions = make_regions(1, &["solid"]);
        let (mut bridge, _rx) = make_bridge(regions.clone(), 1);

        bridge.handle_event(&RunEvent::Region(
            0,
            RegionEvent::Progress {
                pattern: Pattern::SolidBits,
                pass: 1,
                sub_pass: 50,
                total: 0,
            },
        ));

        check!(regions[0].progress_bp.load(Ordering::Relaxed) == 0);
    }

    #[test]
    fn test_complete_advances_pattern_index() {
        let regions = make_regions(1, &["solid", "walk"]);
        let (mut bridge, _rx) = make_bridge(regions.clone(), 1);

        // Start first pattern
        bridge.handle_event(&RunEvent::Region(
            0,
            RegionEvent::TestStart {
                pattern: Pattern::SolidBits,
                pass: 1,
            },
        ));
        check!(regions[0].current_pattern() == "solid");

        // Complete it
        bridge.handle_event(&RunEvent::Region(
            0,
            RegionEvent::TestComplete {
                pattern: Pattern::SolidBits,
                pass: 1,
                elapsed: Duration::from_millis(100),
                bytes: 8192,
                failures: vec![],
            },
        ));

        // Next test start should set pattern index 1
        bridge.handle_event(&RunEvent::Region(
            0,
            RegionEvent::TestStart {
                pattern: Pattern::WalkingOnes,
                pass: 1,
            },
        ));
        check!(regions[0].current_pattern() == "walk");
    }

    #[test]
    fn test_complete_with_failures_sends_errors() {
        use crate::failure::FailureBuilder;

        let regions = make_regions(1, &["solid"]);
        let (mut bridge, rx) = make_bridge(regions.clone(), 1);

        let failure = FailureBuilder::default()
            .addr(0xdead_0000_usize)
            .expected(0xFF_u64)
            .actual(0xFE_u64)
            .build();

        bridge.handle_event(&RunEvent::Region(
            0,
            RegionEvent::TestComplete {
                pattern: Pattern::SolidBits,
                pass: 1,
                elapsed: Duration::from_millis(100),
                bytes: 8192,
                failures: vec![failure],
            },
        ));

        check!(regions[0].failure_count.load(Ordering::Relaxed) == 1);

        match rx.try_recv() {
            Ok(TuiEvent::Failure(f)) => {
                check!(f.region_idx == 0);
                check!(f.expected == 0xFF);
                check!(f.actual == 0xFE);
                check!(f.flipped_bits == crate::tui::FlippedBits::Single(0));
            }
            other => panic!("expected TuiEvent::Failure, got {other:?}"),
        }
    }

    #[test]
    fn pass_complete_resets_pattern_index() {
        let regions = make_regions(1, &["solid", "walk"]);
        let (mut bridge, _rx) = make_bridge(regions.clone(), 2);

        // Simulate completing first pass
        bridge.handle_event(&RunEvent::Region(
            0,
            RegionEvent::TestComplete {
                pattern: Pattern::SolidBits,
                pass: 1,
                elapsed: Duration::from_millis(50),
                bytes: 4096,
                failures: vec![],
            },
        ));
        // pattern_indices[0] is now 1

        bridge.handle_event(&RunEvent::Region(
            0,
            RegionEvent::PassComplete {
                pass: 1,
                failures: 0,
                elapsed: Duration::from_millis(100),
            },
        ));

        // After pass complete, next TestStart should use index 0 again
        bridge.handle_event(&RunEvent::Region(
            0,
            RegionEvent::TestStart {
                pattern: Pattern::SolidBits,
                pass: 2,
            },
        ));
        check!(regions[0].current_pattern() == "solid");
    }

    #[test]
    fn pass_complete_final_pass_sends_region_done() {
        let regions = make_regions(1, &["solid"]);
        let (mut bridge, rx) = make_bridge(regions.clone(), 1);

        bridge.handle_event(&RunEvent::Region(
            0,
            RegionEvent::PassComplete {
                pass: 1,
                failures: 0,
                elapsed: Duration::from_millis(100),
            },
        ));

        match rx.try_recv() {
            Ok(TuiEvent::RegionDone(0)) => {}
            other => panic!("expected TuiEvent::RegionDone(0), got {other:?}"),
        }
    }

    #[test]
    fn pass_complete_non_final_no_region_done() {
        let regions = make_regions(1, &["solid"]);
        let (mut bridge, rx) = make_bridge(regions.clone(), 3);

        bridge.handle_event(&RunEvent::Region(
            0,
            RegionEvent::PassComplete {
                pass: 1,
                failures: 0,
                elapsed: Duration::from_millis(100),
            },
        ));

        check!(rx.try_recv().is_err());
    }

    #[test]
    fn run_complete_returns_false() {
        let regions = make_regions(1, &["solid"]);
        let (mut bridge, _rx) = make_bridge(regions, 1);

        let cont = bridge.handle_event(&RunEvent::RunComplete);
        assert!(!cont);
    }

    #[test]
    fn out_of_bounds_region_ignored() {
        let regions = make_regions(1, &["solid"]);
        let (mut bridge, _rx) = make_bridge(regions.clone(), 1);

        // Region index 5 is out of bounds — should not panic
        let cont = bridge.handle_event(&RunEvent::Region(
            5,
            RegionEvent::TestStart {
                pattern: Pattern::SolidBits,
                pass: 1,
            },
        ));
        assert!(cont);
    }

    #[test]
    fn run_sends_done_for_incomplete_regions() {
        let regions = make_regions(2, &["solid"]);
        let (bridge, rx) = make_bridge(regions, 1);

        let (event_tx, event_rx) = crate::events::event_bus();
        // Send RunComplete without any PassComplete — both regions are incomplete
        event_tx.send(RunEvent::RunComplete).unwrap();
        drop(event_tx);

        bridge.run(&event_rx);

        let mut done_indices = vec![];
        while let Ok(event) = rx.try_recv() {
            if let TuiEvent::RegionDone(idx) = event {
                done_indices.push(idx);
            }
        }
        done_indices.sort_unstable();
        check!(done_indices == vec![0, 1]);
    }

    #[test]
    fn run_no_duplicate_done_for_completed_regions() {
        let regions = make_regions(1, &["solid"]);
        let (bridge, rx) = make_bridge(regions, 1);

        let (event_tx, event_rx) = crate::events::event_bus();
        // Region completes naturally, then RunComplete
        event_tx
            .send(RunEvent::Region(
                0,
                RegionEvent::PassComplete {
                    pass: 1,
                    failures: 0,
                    elapsed: Duration::from_millis(100),
                },
            ))
            .unwrap();
        event_tx.send(RunEvent::RunComplete).unwrap();
        drop(event_tx);

        bridge.run(&event_rx);

        let done_count = std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|e| matches!(e, TuiEvent::RegionDone(_)))
            .count();
        // One from PassComplete, zero from cleanup
        check!(done_count == 1);
    }

    #[test]
    fn multiple_regions_independent_tracking() {
        let regions = make_regions(2, &["solid", "walk"]);
        let (mut bridge, _rx) = make_bridge(regions.clone(), 1);

        // Advance region 0 pattern
        bridge.handle_event(&RunEvent::Region(
            0,
            RegionEvent::TestStart {
                pattern: Pattern::SolidBits,
                pass: 1,
            },
        ));
        bridge.handle_event(&RunEvent::Region(
            0,
            RegionEvent::TestComplete {
                pattern: Pattern::SolidBits,
                pass: 1,
                elapsed: Duration::from_millis(50),
                bytes: 4096,
                failures: vec![],
            },
        ));

        // Region 1 should still be on first pattern
        bridge.handle_event(&RunEvent::Region(
            1,
            RegionEvent::TestStart {
                pattern: Pattern::SolidBits,
                pass: 1,
            },
        ));
        check!(regions[1].current_pattern() == "solid");

        // Region 0 should be on second pattern
        bridge.handle_event(&RunEvent::Region(
            0,
            RegionEvent::TestStart {
                pattern: Pattern::WalkingOnes,
                pass: 1,
            },
        ));
        check!(regions[0].current_pattern() == "walk");
    }

    #[test]
    fn ecc_deltas_handled_without_panic() {
        let regions = make_regions(1, &["solid"]);
        let (mut bridge, _rx) = make_bridge(regions, 1);

        let cont = bridge.handle_event(&RunEvent::Region(
            0,
            RegionEvent::EccDeltas {
                pass: 1,
                deltas: vec![crate::edac::EccDelta {
                    mc: 0,
                    dimm_index: 1,
                    label: Some("DIMM_A1".to_owned()),
                    ce_delta: 2,
                    ue_delta: 0,
                }],
            },
        ));
        assert!(cont);
    }

    #[test]
    fn run_on_disconnect_sends_done_for_incomplete() {
        let regions = make_regions(2, &["solid"]);
        let (bridge, rx) = make_bridge(regions, 1);

        let (event_tx, event_rx) = crate::events::event_bus();
        // Drop sender without sending RunComplete — simulates disconnect
        drop(event_tx);

        bridge.run(&event_rx);

        let mut done_indices = vec![];
        while let Ok(event) = rx.try_recv() {
            if let TuiEvent::RegionDone(idx) = event {
                done_indices.push(idx);
            }
        }
        done_indices.sort_unstable();
        check!(done_indices == vec![0, 1]);
    }
}
