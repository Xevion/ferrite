use std::time::Duration;

use crate::Failure;
use crate::dimm::DimmTopology;
use crate::edac::EccDelta;
use crate::pattern::Pattern;
use crate::phys::MapStats;

/// Top-level event emitted by the unified runner via `crossbeam_channel`.
///
/// These are **internal** event types — unstable, free to change between versions.
/// The stable NDJSON schema (XEV-611) is a curated projection of these.
#[derive(Debug)]
pub enum RunEvent {
    /// Emitted once at the start of a test run.
    RunStart {
        size: usize,
        passes: usize,
        patterns: Vec<Pattern>,
        regions: usize,
    },

    /// Physical address map statistics, emitted after pagemap resolution.
    MapInfo { stats: MapStats },

    /// Installed DIMM topology, emitted when SMBIOS/EDAC data is available.
    DimmInfo { topology: DimmTopology },

    /// A per-region event, tagged with the region index.
    Region(usize, RegionEvent),

    /// Tracing log event injected by a custom subscriber layer.
    Log {
        level: tracing::Level,
        target: String,
        message: String,
        fields: serde_json::Value,
    },

    /// Emitted once when the entire run is complete.
    RunComplete,
}

/// Per-region events emitted during testing.
#[derive(Debug)]
pub enum RegionEvent {
    /// A new pass is starting for this region.
    PassStart { pass: usize, total_passes: usize },

    /// A pattern test is starting within a pass.
    TestStart { pattern: Pattern, pass: usize },

    /// Sub-pass progress update for the current pattern.
    Progress {
        pattern: Pattern,
        pass: usize,
        sub_pass: u64,
        total: u64,
    },

    /// A pattern test completed (may include failures).
    TestComplete {
        pattern: Pattern,
        pass: usize,
        elapsed: Duration,
        bytes: u64,
        failures: Vec<Failure>,
    },

    /// All patterns in a pass finished for this region.
    PassComplete {
        pass: usize,
        failures: usize,
        elapsed: Duration,
    },

    /// ECC counter deltas detected after a pass.
    EccDeltas { pass: usize, deltas: Vec<EccDelta> },
}

/// Shorthand for the sender half of the event bus.
pub type EventTx = crossbeam_channel::Sender<RunEvent>;

/// Shorthand for the receiver half of the event bus.
pub type EventRx = crossbeam_channel::Receiver<RunEvent>;

/// Create an unbounded event bus channel.
///
/// Returns `(sender, receiver)`. The sender can be cloned for multiple
/// producers (e.g. one per region worker thread).
#[must_use]
pub fn event_bus() -> (EventTx, EventRx) {
    crossbeam_channel::unbounded()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use assert2::{assert, check};

    use crate::pattern::Pattern;
    use crate::phys::MapStats;

    use super::*;

    #[test]
    fn event_bus_send_recv() {
        let (tx, rx) = event_bus();
        tx.send(RunEvent::RunStart {
            size: 1024,
            passes: 1,
            patterns: vec![Pattern::SolidBits],
            regions: 1,
        })
        .unwrap();

        let event = rx.recv().unwrap();
        assert!(let RunEvent::RunStart { size: 1024, .. } = event);
    }

    #[test]
    fn event_bus_multiple_events() {
        let (tx, rx) = event_bus();

        tx.send(RunEvent::Region(
            0,
            RegionEvent::PassStart {
                pass: 1,
                total_passes: 2,
            },
        ))
        .unwrap();
        tx.send(RunEvent::Region(
            0,
            RegionEvent::TestStart {
                pattern: Pattern::SolidBits,
                pass: 1,
            },
        ))
        .unwrap();
        tx.send(RunEvent::Region(
            0,
            RegionEvent::TestComplete {
                pattern: Pattern::SolidBits,
                pass: 1,
                elapsed: Duration::from_millis(100),
                bytes: 8192,
                failures: vec![],
            },
        ))
        .unwrap();
        tx.send(RunEvent::RunComplete).unwrap();

        let mut count = 0;
        while let Ok(_event) = rx.try_recv() {
            count += 1;
        }
        check!(count == 4);
    }

    #[test]
    fn cloned_sender_works() {
        let (tx, rx) = event_bus();
        let tx2 = tx.clone();

        tx.send(RunEvent::RunComplete).unwrap();
        tx2.send(RunEvent::RunComplete).unwrap();

        check!(rx.try_recv().is_ok());
        check!(rx.try_recv().is_ok());
        check!(rx.try_recv().is_err());
    }

    #[test]
    fn map_info_event() {
        let (tx, rx) = event_bus();
        tx.send(RunEvent::MapInfo {
            stats: MapStats {
                total_pages: 100,
                huge_pages: 5,
                thp_pages: 10,
                hwpoison_pages: 0,
                unevictable_pages: 90,
            },
        })
        .unwrap();

        let event = rx.recv().unwrap();
        assert!(let RunEvent::MapInfo { .. } = event);
    }

    #[test]
    fn region_event_progress() {
        let (tx, rx) = event_bus();
        tx.send(RunEvent::Region(
            2,
            RegionEvent::Progress {
                pattern: Pattern::WalkingOnes,
                pass: 1,
                sub_pass: 32,
                total: 64,
            },
        ))
        .unwrap();

        let event = rx.recv().unwrap();
        assert!(let RunEvent::Region(2, RegionEvent::Progress { .. }) = event);
    }

    #[test]
    fn region_event_ecc_deltas() {
        let (tx, rx) = event_bus();
        tx.send(RunEvent::Region(
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
        ))
        .unwrap();

        let event = rx.recv().unwrap();
        assert!(let RunEvent::Region(0, RegionEvent::EccDeltas { .. }) = event);
    }

    #[test]
    fn region_event_pass_complete() {
        let (tx, rx) = event_bus();
        tx.send(RunEvent::Region(
            1,
            RegionEvent::PassComplete {
                pass: 1,
                failures: 3,
                elapsed: Duration::from_secs(5),
            },
        ))
        .unwrap();

        let event = rx.recv().unwrap();
        assert!(let RunEvent::Region(1, RegionEvent::PassComplete { .. }) = event);
    }

    #[test]
    fn log_event() {
        let (tx, rx) = event_bus();
        tx.send(RunEvent::Log {
            level: tracing::Level::INFO,
            target: "ferrite::runner".to_owned(),
            message: "test message".to_owned(),
            fields: serde_json::json!({}),
        })
        .unwrap();

        let event = rx.recv().unwrap();
        assert!(let RunEvent::Log { .. } = event);
    }

    #[test]
    fn disconnected_receiver() {
        let (tx, rx) = event_bus();
        drop(rx);
        check!(tx.send(RunEvent::RunComplete).is_err());
    }

    #[test]
    fn disconnected_sender() {
        let (tx, rx) = event_bus();
        drop(tx);
        check!(rx.try_recv().is_err());
    }
}
