use std::any::Any;
use std::panic::{self, AssertUnwindSafe};
use std::time::Instant;

use snafu::Snafu;

use crate::edac::{EccDelta, EdacSnapshot};
use crate::events::{EventTx, RunEvent};
use crate::pattern::{Pattern, run_pattern};
use crate::physmem::phys::PhysResolver;
use crate::shutdown;
use crate::{Failure, FailureBudget};

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
#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum PatternError {
    /// A pattern worker panicked. The test buffer may be in a partially-modified
    /// state and should not be trusted. The message carries the panic payload.
    #[snafu(display("unrecoverable error in pattern execution: {message}"))]
    Unrecoverable { message: String },
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
    /// True if the pattern stopped early due to a quit request, so its
    /// failures (and absence of failures) are incomplete.
    pub interrupted: bool,
    /// True if the pattern hit `--max-errors` and its failure list was
    /// truncated -- more failures existed than were collected.
    pub capped: bool,
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
    /// Resolved worker-thread count for pattern execution; 1 means serial.
    pub workers: usize,
}

/// Complete results of a test run, suitable for serialization and post-processing.
#[derive(Debug, serde::Serialize)]
pub struct RunResults {
    pub config: RunConfig,
    pub passes: Vec<PassResult>,
    #[serde(with = "crate::units::duration_ms")]
    pub elapsed: std::time::Duration,
    pub total_failures: usize,
    /// Fraction of installed physical RAM this run tested. Set by the binary
    /// before rendering; defaults to [`Coverage::Unavailable`].
    pub coverage: crate::physmem::sysmem::Coverage,
    /// Populated by [`crate::error_analysis::analyze`] after the run completes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_analysis: Option<crate::error_analysis::ErrorAnalysis>,
}

impl RunResults {
    /// Build `RunResults` from pass results.
    ///
    /// `coverage` defaults to [`Coverage::Unavailable`]; the binary overwrites
    /// it once the installed-RAM denominator is known.
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
            coverage: crate::physmem::sysmem::Coverage::Unavailable,
            error_analysis: None,
        }
    }
}

/// Run all selected patterns for the given number of passes on the whole test buffer.
///
/// Emits `RunEvent`s via the provided channel as testing proceeds. The caller
/// is responsible for emitting global events (`RunStart`, `RunComplete`,
/// `MapInfo`, etc.).
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
/// `pause` is a neutral pause signal checked at each work-chunk boundary (fused
/// into the activity callback): while it is set, the worker parks instead of
/// advancing. Pass `None` (headless) to never pause. A pending quit always
/// breaks out of a paused wait.
///
/// # Errors
///
/// Returns [`PatternError::Unrecoverable`] if a pattern worker panics. The
/// test buffer should be considered corrupted after this error.
#[expect(
    clippy::too_many_arguments,
    reason = "one entry point threads buffer, selection, event bus, resolver, activity, and pause; a params struct would only relocate the coupling"
)]
pub fn run(
    buf: &mut [u64],
    patterns: &[Pattern],
    passes: usize,
    parallel: bool,
    max_errors: usize,
    tx: &EventTx,
    resolver: Option<&(dyn PhysResolver + Sync)>,
    on_activity: &(dyn Fn(f64) + Sync),
    pause: crate::pause::PauseSignal<'_>,
) -> Result<Vec<PassResult>, PatternError> {
    let buf_bytes = buf.len() as u64 * 8;
    let mut results = Vec::with_capacity(passes);

    // Fuse the pause check into activity reporting: both fire at the same
    // per-chunk granularity in every pattern (march and the ops callbacks), so
    // parking here blocks work between chunks without any pattern/ops changes.
    let paused_activity = move |pos: f64| {
        on_activity(pos);
        crate::pause::wait_while_paused(pause);
    };

    for pass in 0..passes {
        if shutdown::quit_requested() {
            break;
        }

        let pass_start = Instant::now();
        let _ = tx.send(RunEvent::PassStart {
            pass: pass + 1,
            total_passes: passes,
        });

        let edac_before = EdacSnapshot::capture();

        let mut pattern_results = Vec::with_capacity(patterns.len());
        for &pattern in patterns {
            if shutdown::quit_requested() {
                break;
            }
            let sub_passes = pattern.sub_passes();

            let _ = tx.send(RunEvent::TestStart {
                pattern,
                pass: pass + 1,
            });

            let start = Instant::now();

            // One budget per pattern: caps total collected failures so a wholly
            // bad DIMM cannot exhaust memory materializing one record per word.
            let budget = FailureBudget::new(max_errors);
            let mut sub_pass_count: u64 = 0;
            // SAFETY: captured state (sub_pass_count, tx) is not used after
            // an unwind -- we return Err immediately on panic.
            let pattern_result = panic::catch_unwind(AssertUnwindSafe(|| {
                run_pattern(
                    pattern,
                    buf,
                    parallel,
                    &budget,
                    &mut || {
                        sub_pass_count += 1;
                        let _ = tx.send(RunEvent::Progress {
                            pattern,
                            pass: pass + 1,
                            sub_pass: sub_pass_count,
                            total: sub_passes,
                        });
                    },
                    &paused_activity,
                )
            }));
            let mut failures = match pattern_result {
                Ok(f) => f,
                Err(panic_val) => {
                    return Err(PatternError::Unrecoverable {
                        message: extract_panic_msg(&panic_val).to_owned(),
                    });
                }
            };
            let elapsed = start.elapsed();
            let bytes_processed = buf_bytes * 2 * pattern.sub_passes();
            // A quit observed now means the pattern's inner loop bailed out
            // early, so its results are partial.
            let interrupted = shutdown::quit_requested();
            let capped = budget.overflowed();

            if let Some(resolver) = resolver {
                for f in &mut failures {
                    f.phys_addr = resolver.resolve(f.addr).ok();
                }
            }

            let _ = tx.send(RunEvent::TestComplete {
                pattern,
                pass: pass + 1,
                elapsed,
                bytes: bytes_processed,
                failures: failures.clone(),
                interrupted,
                capped,
            });

            pattern_results.push(PatternResult {
                pattern,
                failures,
                elapsed,
                bytes_processed,
                interrupted,
                capped,
            });
        }

        let ecc_deltas = match (&edac_before, EdacSnapshot::capture()) {
            (Some(before), Some(after)) => {
                let deltas = before.delta(&after);
                if !deltas.is_empty() {
                    let _ = tx.send(RunEvent::EccDeltas {
                        pass: pass + 1,
                        deltas: deltas.clone(),
                    });
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

        let _ = tx.send(RunEvent::PassComplete {
            pass: pass + 1,
            failures: total,
            elapsed: pass_elapsed,
        });

        results.push(pass_result);
    }

    Ok(results)
}

/// Turn a run's raw pass results into finalized [`RunResults`].
///
/// This is the shared tail of every run path: assemble results from the pass
/// list, record the measured single-run coverage, classify bit errors, merge
/// the run into the cross-run coverage store (attaching cumulative stats), and
/// classify the untested remainder.
///
/// Callers own the parts that genuinely differ between run modes: driving
/// [`run`] itself (which thread it executes on), the event-consumer wiring, the
/// `RunComplete` emission, and any NDJSON summary emission. Those are passed in
/// here only as their already-computed products (`pass_results`, `elapsed`,
/// `coverage`).
#[must_use]
pub fn execute_run(
    pass_results: Vec<PassResult>,
    config: RunConfig,
    elapsed: std::time::Duration,
    coverage: crate::physmem::sysmem::Coverage,
    coverage_ctx: Option<crate::physmem::lifecycle::CoverageCtx>,
    run_ranges: Option<Vec<crate::physmem::pfn::PfnRange>>,
) -> RunResults {
    let mut results = RunResults::from_passes(pass_results, config, elapsed);
    results.coverage = coverage;
    crate::error_analysis::analyze(&mut results);
    let covered =
        crate::physmem::lifecycle::finalize_coverage(coverage_ctx, run_ranges, &mut results);
    crate::physmem::lifecycle::attach_gap_classification(covered, &mut results);
    results
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};
    use serial_test::serial;

    use crate::events::{self, RunEvent};
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
                    interrupted: false,
                    capped: false,
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
                    interrupted: false,
                    capped: false,
                },
            ],
            ecc_deltas: vec![],
        };
        check!(pr.total_failures() == 3);
    }

    mod run_results {
        use std::time::Duration;

        use assert2::{assert, check};

        use crate::failure::FailureBuilder;

        use super::*;

        fn config() -> RunConfig {
            RunConfig {
                size: 8192,
                passes: 1,
                patterns: vec![Pattern::SolidBits],
                workers: 1,
            }
        }

        fn pass_with_failures(pass_number: usize, n: usize) -> PassResult {
            let failures = (0..n)
                .map(|i| {
                    FailureBuilder::default()
                        .addr(i * 8)
                        .expected(0)
                        .actual(1)
                        .build()
                })
                .collect();
            PassResult {
                pass_number,
                pattern_results: vec![PatternResult {
                    pattern: Pattern::SolidBits,
                    failures,
                    elapsed: Duration::from_millis(10),
                    bytes_processed: 8192,
                    interrupted: false,
                    capped: false,
                }],
                ecc_deltas: vec![],
            }
        }

        #[test]
        fn from_passes_totals_failures() {
            let results = RunResults::from_passes(
                vec![pass_with_failures(1, 2)],
                config(),
                Duration::from_millis(10),
            );
            check!(results.passes.len() == 1);
            check!(results.total_failures == 2);
        }

        #[test]
        fn from_passes_sums_across_multiple_passes() {
            let results = RunResults::from_passes(
                vec![pass_with_failures(1, 1), pass_with_failures(2, 3)],
                config(),
                Duration::from_millis(10),
            );
            check!(results.total_failures == 4);
        }

        #[test]
        fn serializes_passes_dimension() {
            let results = RunResults::from_passes(
                vec![pass_with_failures(1, 0), pass_with_failures(2, 1)],
                config(),
                Duration::from_millis(10),
            );
            let json = serde_json::to_value(&results).unwrap();
            assert!(json["passes"].is_array());
            check!(json["passes"][0]["pass_number"] == 1);
            assert!(json.get("regions").is_none());
        }

        #[test]
        fn coverage_defaults_to_unavailable() {
            let results = RunResults::from_passes(
                vec![pass_with_failures(1, 0)],
                config(),
                Duration::from_millis(10),
            );
            check!(results.coverage == crate::physmem::sysmem::Coverage::Unavailable);
            let json = serde_json::to_value(&results).unwrap();
            check!(json["coverage"]["status"] == "unavailable");
        }
    }

    mod execute_run {
        use assert2::check;

        use super::*;
        use crate::physmem::sysmem::Coverage;

        fn config() -> RunConfig {
            RunConfig {
                size: 4096,
                passes: 1,
                patterns: vec![Pattern::SolidBits],
                workers: 1,
            }
        }

        #[test]
        fn assembles_results_and_sets_coverage() {
            let results = super::super::execute_run(
                vec![],
                config(),
                std::time::Duration::ZERO,
                Coverage::Unavailable,
                None,
                None,
            );
            check!(results.total_failures == 0);
            check!(results.coverage == Coverage::Unavailable);
        }
    }

    #[test]
    fn run_single_pass_clean_memory() {
        let mut buf = vec![0u64; 1024];
        let (tx, rx) = make_tx();
        let results = run(
            &mut buf,
            &[Pattern::SolidBits],
            1,
            false,
            0,
            &tx,
            None,
            &|_| {},
            None,
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
        assert!(let RunEvent::PassStart { .. } = &events[0]);
    }

    #[test]
    fn run_multi_pass() {
        let mut buf = vec![0u64; 1024];
        let (tx, _rx) = make_tx();
        let results = run(
            &mut buf,
            &[Pattern::SolidBits],
            3,
            false,
            0,
            &tx,
            None,
            &|_| {},
            None,
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
        let results = run(
            &mut buf,
            Pattern::ALL,
            1,
            false,
            0,
            &tx,
            None,
            &|_| {},
            None,
        )
        .unwrap();
        assert!(results.len() == 1);
        check!(results[0].pattern_results.len() == Pattern::ALL.len());
        check!(results[0].total_failures() == 0);
    }

    #[test]
    fn run_empty_patterns() {
        let mut buf = vec![0u64; 1024];
        let (tx, _rx) = make_tx();
        let results = run(&mut buf, &[], 1, false, 0, &tx, None, &|_| {}, None).unwrap();
        check!(results.len() == 1);
        check!(results[0].pattern_results.is_empty());
        check!(results[0].total_failures() == 0);
    }

    #[test]
    fn run_parallel_clean() {
        let mut buf = vec![0u64; 4096];
        let (tx, _rx) = make_tx();
        let results = run(&mut buf, Pattern::ALL, 1, true, 0, &tx, None, &|_| {}, None).unwrap();
        assert!(results.len() == 1);
        check!(results[0].total_failures() == 0);
    }

    /// Minimal resolver that always succeeds -- for testing the resolver branch.
    struct StubResolver;

    #[cfg_attr(coverage_nightly, coverage(off))]
    impl crate::physmem::phys::PhysResolver for StubResolver {
        fn build_map(
            &mut self,
            _base: usize,
            _len: usize,
        ) -> Result<crate::physmem::phys::MapStats, crate::physmem::phys::PhysError> {
            unreachable!()
        }
        fn resolve(
            &self,
            vaddr: usize,
        ) -> Result<crate::physmem::phys::PhysAddr, crate::physmem::phys::PhysError> {
            Ok(crate::physmem::phys::PhysAddr(vaddr as u64 + 0x1_0000_0000))
        }
        fn page_flags(
            &self,
            _pfn: u64,
        ) -> Result<crate::physmem::kpageflags::KPageFlags, crate::physmem::phys::PhysError>
        {
            Ok(crate::physmem::kpageflags::KPageFlags::default())
        }
        fn verify_stability(
            &self,
            _base: usize,
            _len: usize,
        ) -> Result<usize, crate::physmem::phys::PhysError> {
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
            &[Pattern::SolidBits],
            1,
            false,
            0,
            &tx,
            Some(&resolver),
            &|_| {},
            None,
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
            let e = PatternError::Unrecoverable {
                message: "worker panicked".to_owned(),
            };
            assert!(e.to_string().contains("unrecoverable"));
            assert!(e.to_string().contains("worker panicked"));
        }
    }

    #[test]
    #[serial]
    fn pattern_marked_interrupted_on_mid_pattern_quit() {
        shutdown::reset();
        let mut buf = vec![0u64; 1024];
        let (tx, _rx) = make_tx();
        // Request quit from inside the pattern via the activity callback, so the
        // quit lands mid-pattern rather than at the between-pattern boundary.
        let results = run(
            &mut buf,
            &[Pattern::WalkingOnes],
            1,
            false,
            0,
            &tx,
            None,
            &|_| shutdown::request_quit(shutdown::QuitReason::UserQuit),
            None,
        )
        .unwrap();
        shutdown::reset();
        check!(results.len() == 1);
        check!(results[0].pattern_results.len() == 1);
        check!(results[0].pattern_results[0].interrupted);
    }

    #[test]
    #[serial]
    fn pattern_not_interrupted_on_clean_completion() {
        shutdown::reset();
        let mut buf = vec![0u64; 1024];
        let (tx, _rx) = make_tx();
        let results = run(
            &mut buf,
            &[Pattern::SolidBits],
            1,
            false,
            0,
            &tx,
            None,
            &|_| {},
            None,
        )
        .unwrap();
        check!(!results[0].pattern_results[0].interrupted);
    }

    #[test]
    #[serial]
    fn run_respects_quit_flag() {
        shutdown::reset();
        shutdown::request_quit(shutdown::QuitReason::UserQuit);
        let mut buf = vec![0u64; 1024];
        let (tx, _rx) = make_tx();
        let results = run(
            &mut buf,
            Pattern::ALL,
            100,
            false,
            0,
            &tx,
            None,
            &|_| {},
            None,
        )
        .unwrap();
        check!(results.is_empty());
    }

    #[test]
    fn emits_expected_event_sequence() {
        let mut buf = vec![0u64; 1024];
        let (tx, rx) = make_tx();
        let _ = run(
            &mut buf,
            &[Pattern::SolidBits],
            1,
            false,
            0,
            &tx,
            None,
            &|_| {},
            None,
        )
        .unwrap();
        drop(tx);

        let events: Vec<_> = rx.try_iter().collect();

        // PassStart, TestStart, Progress..., TestComplete, PassComplete
        assert!(let RunEvent::PassStart { pass: 1, total_passes: 1 } = &events[0]);
        assert!(let RunEvent::TestStart { pattern: Pattern::SolidBits, pass: 1 } = &events[1]);

        // Last two should be TestComplete and PassComplete
        let last = &events[events.len() - 1];
        assert!(let RunEvent::PassComplete { pass: 1, .. } = last);
        let second_last = &events[events.len() - 2];
        assert!(let RunEvent::TestComplete { pattern: Pattern::SolidBits, .. } = second_last);
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
