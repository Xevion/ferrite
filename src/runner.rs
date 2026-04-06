use std::any::Any;
use std::panic::{self, AssertUnwindSafe};
use std::time::Instant;

use anyhow::anyhow;
use thiserror::Error;

use crate::Failure;
use crate::edac::{EccDelta, EdacSnapshot};
use crate::events::{EventTx, RegionEvent, RunEvent};
use crate::pattern::{Pattern, run_pattern};
use crate::phys::PhysResolver;
use crate::shutdown;

/// Extract a human-readable message from a panic payload.
///
/// Handles the two common payload types (`&str` literal and `String`) and
/// falls back to `"unknown panic"` for any other type.
fn extract_panic_msg(val: &Box<dyn Any + Send>) -> &str {
    val.downcast_ref::<&str>()
        .copied()
        .or_else(|| val.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("unknown panic")
}

/// Unrecoverable error from the test runner -- indicates a panic in a pattern
/// worker that could not be handled gracefully.
#[derive(Debug, Error)]
pub enum PatternError {
    /// A pattern worker panicked. The test buffer may be in a partially-modified
    /// state and should not be trusted. The inner error carries the panic message.
    #[error("unrecoverable error in pattern execution: {0}")]
    Unrecoverable(#[source] anyhow::Error),
}

/// Result of running a single pattern.
#[derive(Debug, serde::Serialize)]
pub struct PatternResult {
    pub pattern: Pattern,
    pub failures: Vec<Failure>,
    #[serde(with = "crate::units::duration_ms")]
    pub elapsed: std::time::Duration,
    /// Total bytes touched (writes + reads across all sub-passes).
    pub bytes_processed: u64,
}

/// Result of a full pass (all patterns).
#[derive(Debug, serde::Serialize)]
pub struct PassResult {
    pub pass_number: usize,
    pub pattern_results: Vec<PatternResult>,
    pub ecc_deltas: Vec<EccDelta>,
}

impl PassResult {
    #[must_use]
    pub fn total_failures(&self) -> usize {
        self.pattern_results.iter().map(|r| r.failures.len()).sum()
    }
}

/// Configuration snapshot captured at the start of a run.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RunConfig {
    pub size: usize,
    pub passes: usize,
    pub patterns: Vec<Pattern>,
    pub regions: usize,
    pub parallel: bool,
}

/// Complete results of a test run, suitable for serialization and post-processing.
#[derive(Debug, serde::Serialize)]
pub struct RunResults {
    pub config: RunConfig,
    pub passes: Vec<PassResult>,
    #[serde(with = "crate::units::duration_ms")]
    pub elapsed: std::time::Duration,
    pub total_failures: usize,
    /// Populated by [`crate::error_analysis::analyze`] after the run completes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_analysis: Option<crate::error_analysis::ErrorAnalysis>,
}

impl RunResults {
    /// Build `RunResults` from pass results and configuration.
    #[must_use]
    pub fn from_passes(
        passes: Vec<PassResult>,
        config: RunConfig,
        elapsed: std::time::Duration,
    ) -> Self {
        let total_failures = passes.iter().map(PassResult::total_failures).sum();
        Self {
            config,
            passes,
            elapsed,
            total_failures,
            error_analysis: None,
        }
    }
}

/// Run all selected patterns for the given number of passes on a single buffer region.
///
/// Emits `RunEvent::Region(region_idx, ...)` events via the provided channel as
/// testing proceeds. The caller is responsible for emitting global events
/// (`RunStart`, `RunComplete`, `MapInfo`, etc.).
///
/// When `parallel` is true, each pattern's write and verify phases run across
/// all available CPU cores via Rayon. Pass `false` to force single-threaded
/// execution (useful for benchmarking or on systems where parallelism causes
/// cache interference).
///
/// `on_activity` is called from worker threads with a position (0.0..1.0)
/// within the buffer, suitable for driving activity heatmaps. Pass `&|_| {}`
/// if no activity tracking is needed.
///
/// # Errors
///
/// Returns [`PatternError::Unrecoverable`] if a pattern worker panics. The
/// test buffer should be considered corrupted after this error.
#[allow(clippy::too_many_arguments)]
pub fn run(
    buf: &mut [u64],
    region_idx: usize,
    patterns: &[Pattern],
    passes: usize,
    parallel: bool,
    tx: &EventTx,
    resolver: Option<&(dyn PhysResolver + Sync)>,
    on_activity: &(dyn Fn(f64) + Sync),
) -> Result<Vec<PassResult>, PatternError> {
    let buf_bytes = buf.len() as u64 * 8;
    let mut results = Vec::with_capacity(passes);

    for pass in 0..passes {
        if shutdown::quit_requested() {
            break;
        }

        let pass_start = Instant::now();
        let _ = tx.send(RunEvent::Region(
            region_idx,
            RegionEvent::PassStart {
                pass: pass + 1,
                total_passes: passes,
            },
        ));

        let edac_before = EdacSnapshot::capture();

        let mut pattern_results = Vec::with_capacity(patterns.len());
        for &pattern in patterns {
            if shutdown::quit_requested() {
                break;
            }
            let sub_passes = pattern.sub_passes();

            let _ = tx.send(RunEvent::Region(
                region_idx,
                RegionEvent::TestStart {
                    pattern,
                    pass: pass + 1,
                },
            ));

            let start = Instant::now();

            let mut sub_pass_count: u64 = 0;
            // SAFETY: captured state (sub_pass_count, tx) is not used after
            // an unwind -- we return Err immediately on panic.
            let pattern_result = panic::catch_unwind(AssertUnwindSafe(|| {
                run_pattern(
                    pattern,
                    buf,
                    parallel,
                    &mut || {
                        sub_pass_count += 1;
                        let _ = tx.send(RunEvent::Region(
                            region_idx,
                            RegionEvent::Progress {
                                pattern,
                                pass: pass + 1,
                                sub_pass: sub_pass_count,
                                total: sub_passes,
                            },
                        ));
                    },
                    on_activity,
                )
            }));
            let mut failures = match pattern_result {
                Ok(f) => f,
                Err(panic_val) => {
                    return Err(PatternError::Unrecoverable(anyhow!(
                        "{}",
                        extract_panic_msg(&panic_val)
                    )));
                }
            };
            let elapsed = start.elapsed();
            let bytes_processed = buf_bytes * 2 * pattern.sub_passes();

            if let Some(resolver) = resolver {
                for f in &mut failures {
                    f.phys_addr = resolver.resolve(f.addr).ok();
                }
            }

            let _ = tx.send(RunEvent::Region(
                region_idx,
                RegionEvent::TestComplete {
                    pattern,
                    pass: pass + 1,
                    elapsed,
                    bytes: bytes_processed,
                    failures: failures.clone(),
                },
            ));

            pattern_results.push(PatternResult {
                pattern,
                failures,
                elapsed,
                bytes_processed,
            });
        }

        let ecc_deltas = match (&edac_before, EdacSnapshot::capture()) {
            (Some(before), Some(after)) => {
                let deltas = before.delta(&after);
                if !deltas.is_empty() {
                    let _ = tx.send(RunEvent::Region(
                        region_idx,
                        RegionEvent::EccDeltas {
                            pass: pass + 1,
                            deltas: deltas.clone(),
                        },
                    ));
                }
                deltas
            }
            _ => Vec::new(),
        };

        let pass_result = PassResult {
            pass_number: pass + 1,
            pattern_results,
            ecc_deltas,
        };
        let total = pass_result.total_failures();
        let pass_elapsed = pass_start.elapsed();

        let _ = tx.send(RunEvent::Region(
            region_idx,
            RegionEvent::PassComplete {
                pass: pass + 1,
                failures: total,
                elapsed: pass_elapsed,
            },
        ));

        results.push(pass_result);
    }

    Ok(results)
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};
    use serial_test::serial;

    use crate::events::{self, RegionEvent, RunEvent};
    use crate::pattern::Pattern;

    use super::*;

    fn make_tx() -> (EventTx, events::EventRx) {
        events::event_bus()
    }

    #[test]
    fn total_failures_empty() {
        let pr = PassResult {
            pass_number: 1,
            pattern_results: vec![],
            ecc_deltas: vec![],
        };
        check!(pr.total_failures() == 0);
    }

    #[test]
    fn total_failures_aggregates() {
        let pr = PassResult {
            pass_number: 1,
            pattern_results: vec![
                PatternResult {
                    pattern: Pattern::SolidBits,
                    failures: vec![
                        crate::Failure {
                            addr: 0,
                            expected: 0,
                            actual: 1,
                            word_index: 0,
                            phys_addr: None,
                        },
                        crate::Failure {
                            addr: 8,
                            expected: 0,
                            actual: 1,
                            word_index: 1,
                            phys_addr: None,
                        },
                    ],
                    elapsed: std::time::Duration::ZERO,
                    bytes_processed: 0,
                },
                PatternResult {
                    pattern: Pattern::Checkerboard,
                    failures: vec![crate::Failure {
                        addr: 16,
                        expected: 0,
                        actual: 1,
                        word_index: 2,
                        phys_addr: None,
                    }],
                    elapsed: std::time::Duration::ZERO,
                    bytes_processed: 0,
                },
            ],
            ecc_deltas: vec![],
        };
        check!(pr.total_failures() == 3);
    }

    #[test]
    fn run_single_pass_clean_memory() {
        let mut buf = vec![0u64; 1024];
        let (tx, rx) = make_tx();
        let results = run(
            &mut buf,
            0,
            &[Pattern::SolidBits],
            1,
            false,
            &tx,
            None,
            &|_| {},
        )
        .unwrap();
        drop(tx);

        check!(results.len() == 1);
        check!(results[0].pass_number == 1);
        check!(results[0].total_failures() == 0);
        check!(results[0].pattern_results.len() == 1);
        check!(results[0].pattern_results[0].pattern == Pattern::SolidBits);

        // Verify expected events were emitted
        let events: Vec<_> = rx.try_iter().collect();
        assert!(!events.is_empty());
        assert!(let RunEvent::Region(0, RegionEvent::PassStart { .. }) = &events[0]);
    }

    #[test]
    fn run_multi_pass() {
        let mut buf = vec![0u64; 1024];
        let (tx, _rx) = make_tx();
        let results = run(
            &mut buf,
            0,
            &[Pattern::SolidBits],
            3,
            false,
            &tx,
            None,
            &|_| {},
        )
        .unwrap();
        check!(results.len() == 3);
        for (i, r) in results.iter().enumerate() {
            check!(r.pass_number == i + 1);
            check!(r.total_failures() == 0);
        }
    }

    #[test]
    fn run_all_patterns_clean() {
        let mut buf = vec![0u64; 1024];
        let (tx, _rx) = make_tx();
        let results = run(&mut buf, 0, Pattern::ALL, 1, false, &tx, None, &|_| {}).unwrap();
        assert!(results.len() == 1);
        check!(results[0].pattern_results.len() == Pattern::ALL.len());
        check!(results[0].total_failures() == 0);
    }

    #[test]
    fn run_empty_patterns() {
        let mut buf = vec![0u64; 1024];
        let (tx, _rx) = make_tx();
        let results = run(&mut buf, 0, &[], 1, false, &tx, None, &|_| {}).unwrap();
        check!(results.len() == 1);
        check!(results[0].pattern_results.is_empty());
        check!(results[0].total_failures() == 0);
    }

    #[test]
    fn run_parallel_clean() {
        let mut buf = vec![0u64; 4096];
        let (tx, _rx) = make_tx();
        let results = run(&mut buf, 0, Pattern::ALL, 1, true, &tx, None, &|_| {}).unwrap();
        assert!(results.len() == 1);
        check!(results[0].total_failures() == 0);
    }

    /// Minimal resolver that always succeeds -- for testing the resolver branch.
    struct StubResolver;

    #[cfg_attr(coverage_nightly, coverage(off))]
    impl crate::phys::PhysResolver for StubResolver {
        fn build_map(
            &mut self,
            _base: usize,
            _len: usize,
        ) -> Result<crate::phys::MapStats, crate::phys::PhysError> {
            unreachable!()
        }
        fn resolve(&self, vaddr: usize) -> Result<crate::phys::PhysAddr, crate::phys::PhysError> {
            Ok(crate::phys::PhysAddr(vaddr as u64 + 0x1_0000_0000))
        }
        fn page_flags(&self, _pfn: u64) -> Result<crate::phys::PageFlags, crate::phys::PhysError> {
            Ok(crate::phys::PageFlags::default())
        }
        fn verify_stability(
            &self,
            _base: usize,
            _len: usize,
        ) -> Result<usize, crate::phys::PhysError> {
            Ok(0)
        }
    }

    #[test]
    fn run_with_resolver() {
        let mut buf = vec![0u64; 1024];
        let resolver = StubResolver;
        let (tx, _rx) = make_tx();
        let results = run(
            &mut buf,
            0,
            &[Pattern::SolidBits],
            1,
            false,
            &tx,
            Some(&resolver),
            &|_| {},
        )
        .unwrap();
        check!(results.len() == 1);
        check!(results[0].total_failures() == 0);
    }

    mod pattern_error {
        use assert2::assert;

        use super::*;

        #[test]
        fn pattern_error_display() {
            let e = PatternError::Unrecoverable(anyhow::anyhow!("worker panicked"));
            assert!(e.to_string().contains("unrecoverable"));
            assert!(e.to_string().contains("worker panicked"));
        }
    }

    #[test]
    #[serial]
    fn run_respects_quit_flag() {
        shutdown::reset();
        shutdown::request_quit(shutdown::QuitReason::UserQuit);
        let mut buf = vec![0u64; 1024];
        let (tx, _rx) = make_tx();
        let results = run(&mut buf, 0, Pattern::ALL, 100, false, &tx, None, &|_| {}).unwrap();
        check!(results.is_empty());
    }

    #[test]
    fn emits_expected_event_sequence() {
        let mut buf = vec![0u64; 1024];
        let (tx, rx) = make_tx();
        let _ = run(
            &mut buf,
            7,
            &[Pattern::SolidBits],
            1,
            false,
            &tx,
            None,
            &|_| {},
        )
        .unwrap();
        drop(tx);

        let events: Vec<_> = rx.try_iter().collect();

        // PassStart, TestStart, Progress..., TestComplete, PassComplete
        assert!(let RunEvent::Region(7, RegionEvent::PassStart { pass: 1, total_passes: 1 }) = &events[0]);
        assert!(let RunEvent::Region(7, RegionEvent::TestStart { pattern: Pattern::SolidBits, pass: 1 }) = &events[1]);

        // Last two should be TestComplete and PassComplete
        let last = &events[events.len() - 1];
        assert!(let RunEvent::Region(7, RegionEvent::PassComplete { pass: 1, .. }) = last);
        let second_last = &events[events.len() - 2];
        assert!(let RunEvent::Region(7, RegionEvent::TestComplete { pattern: Pattern::SolidBits, .. }) = second_last);
    }

    #[test]
    fn region_idx_propagated() {
        let mut buf = vec![0u64; 1024];
        let (tx, rx) = make_tx();
        let _ = run(
            &mut buf,
            42,
            &[Pattern::SolidBits],
            1,
            false,
            &tx,
            None,
            &|_| {},
        )
        .unwrap();
        drop(tx);

        for event in rx.try_iter() {
            match event {
                RunEvent::Region(idx, _) => assert!(idx == 42),
                _ => panic!("unexpected non-region event"),
            }
        }
    }

    mod extract_panic_msg_tests {
        use assert2::assert;

        use super::*;

        #[test]
        fn str_payload() {
            let val = std::panic::catch_unwind(|| panic!("str payload")).unwrap_err();
            assert!(extract_panic_msg(&val) == "str payload");
        }

        #[test]
        fn string_payload() {
            let val =
                std::panic::catch_unwind(|| std::panic::panic_any(String::from("owned string")))
                    .unwrap_err();
            assert!(extract_panic_msg(&val) == "owned string");
        }

        #[test]
        fn unknown_payload() {
            let val = std::panic::catch_unwind(|| std::panic::panic_any(42u32)).unwrap_err();
            assert!(extract_panic_msg(&val) == "unknown panic");
        }
    }
}
