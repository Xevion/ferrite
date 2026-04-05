use std::any::Any;
use std::panic::{self, AssertUnwindSafe};
use std::time::Instant;

use anyhow::anyhow;
use indicatif::{ProgressBar, ProgressStyle};
use thiserror::Error;

use crate::Failure;
use crate::edac::{EccDelta, EdacSnapshot};
use crate::output::OutputSink;
use crate::pattern::{Pattern, run_pattern};
use crate::phys::PhysResolver;

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

/// Unrecoverable error from the test runner — indicates a panic in a pattern
/// worker that could not be handled gracefully.
#[derive(Debug, Error)]
pub enum PatternError {
    /// A pattern worker panicked. The test buffer may be in a partially-modified
    /// state and should not be trusted. The inner error carries the panic message.
    #[error("unrecoverable error in pattern execution: {0}")]
    Unrecoverable(#[source] anyhow::Error),
}

/// Result of running a single pattern.
pub struct PatternResult {
    pub pattern: Pattern,
    pub failures: Vec<Failure>,
    pub elapsed: std::time::Duration,
    /// Total bytes touched (writes + reads across all sub-passes).
    pub bytes_processed: u64,
}

/// Result of a full pass (all patterns).
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

/// Run all selected patterns for the given number of passes.
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
///
/// # Panics
///
/// Panics if progress bar template formatting fails (indicates a bug in the
/// hardcoded template string).
pub fn run(
    buf: &mut [u64],
    patterns: &[Pattern],
    passes: usize,
    parallel: bool,
    sink: &mut OutputSink,
    resolver: Option<&dyn PhysResolver>,
    on_activity: &(dyn Fn(f64) + Sync),
) -> Result<Vec<PassResult>, PatternError> {
    // Clone the MultiProgress handle upfront so we don't hold an immutable
    // borrow on `sink` across mutable calls. indicatif's MultiProgress is
    // internally Arc-backed, so cloning is cheap.
    let mp = sink.multi_progress().clone();
    let pass_style =
        ProgressStyle::with_template("{prefix} [{bar:30.cyan/dim}] {pos}/{len} patterns  {msg}")
            .unwrap()
            .progress_chars("=> ");
    let sub_style =
        ProgressStyle::with_template("  {prefix:<20} [{bar:30.yellow/dim}] {pos}/{len}")
            .unwrap()
            .progress_chars("=> ");

    let buf_bytes = buf.len() as u64 * 8;
    sink.print_banner(buf_bytes as usize, passes, patterns.len(), parallel);

    let mut results = Vec::with_capacity(passes);

    for pass in 0..passes {
        let pass_start = Instant::now();
        sink.emit_pass_start(pass + 1, passes);

        // Snapshot EDAC counters before this pass
        let edac_before = EdacSnapshot::capture();

        let pass_pb = mp.add(ProgressBar::new(patterns.len() as u64));
        pass_pb.set_style(pass_style.clone());
        pass_pb.set_prefix(format!("Pass {}/{}", pass + 1, passes));

        let mut pattern_results = Vec::with_capacity(patterns.len());
        for &pattern in patterns {
            let sub_passes = pattern.sub_passes();

            sink.emit_test_start(pattern, pass + 1);

            let inner_pb = if sub_passes > 1 {
                let pb = mp.insert_after(&pass_pb, ProgressBar::new(sub_passes));
                pb.set_style(sub_style.clone());
                pb.set_prefix(pattern.to_string());
                Some(pb)
            } else {
                None
            };

            pass_pb.set_message(format!("{pattern}"));

            let start = Instant::now();

            let mut sub_pass_count: u64 = 0;
            // SAFETY: captured state (sub_pass_count, inner_pb, sink) is not
            // used after an unwind — we return Err immediately on panic.
            let pattern_result = panic::catch_unwind(AssertUnwindSafe(|| {
                run_pattern(
                    pattern,
                    buf,
                    parallel,
                    &mut || {
                        sub_pass_count += 1;
                        if let Some(pb) = &inner_pb {
                            pb.inc(1);
                        }
                        sink.emit_progress(pattern, pass + 1, sub_pass_count, sub_passes);
                    },
                    on_activity,
                )
            }));
            let mut failures = match pattern_result {
                Ok(f) => f,
                Err(panic_val) => {
                    if let Some(pb) = inner_pb {
                        pb.finish_and_clear();
                    }
                    pass_pb.finish_and_clear();
                    return Err(PatternError::Unrecoverable(anyhow!(
                        "{}",
                        extract_panic_msg(&panic_val)
                    )));
                }
            };
            let elapsed = start.elapsed();
            let bytes_processed = buf_bytes * 2 * pattern.sub_passes();

            // Post-process: resolve physical addresses for any failures
            if let Some(resolver) = resolver {
                for f in &mut failures {
                    f.phys_addr = resolver.resolve(f.addr).ok();
                }
            }

            if let Some(pb) = inner_pb {
                pb.finish_and_clear();
            }

            sink.emit_test_complete(pattern, pass + 1, elapsed, bytes_processed, &failures);
            sink.print_test_result(pattern, elapsed, bytes_processed, &failures, &pass_pb);

            pattern_results.push(PatternResult {
                pattern,
                failures,
                elapsed,
                bytes_processed,
            });
            pass_pb.inc(1);
        }
        pass_pb.finish_and_clear();

        // Compute EDAC deltas for this pass
        let ecc_deltas = match (&edac_before, EdacSnapshot::capture()) {
            (Some(before), Some(after)) => {
                let deltas = before.delta(&after);
                if !deltas.is_empty() {
                    sink.emit_ecc_deltas(pass + 1, &deltas);
                    sink.print_ecc_deltas(pass + 1, &deltas);
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

        sink.emit_pass_complete(pass + 1, total, pass_elapsed);
        sink.print_pass_summary(pass + 1, passes, total);

        results.push(pass_result);
    }

    Ok(results)
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};

    use crate::output::OutputSink;
    use crate::pattern::Pattern;
    use crate::units::UnitSystem;

    use super::*;

    fn make_sink() -> OutputSink {
        OutputSink::human(UnitSystem::Binary)
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
        let mut sink = make_sink();
        let results = run(
            &mut buf,
            &[Pattern::SolidBits],
            1,
            false,
            &mut sink,
            None,
            &|_| {},
        )
        .unwrap();
        check!(results.len() == 1);
        check!(results[0].pass_number == 1);
        check!(results[0].total_failures() == 0);
        check!(results[0].pattern_results.len() == 1);
        check!(results[0].pattern_results[0].pattern == Pattern::SolidBits);
    }

    #[test]
    fn run_multi_pass() {
        let mut buf = vec![0u64; 1024];
        let mut sink = make_sink();
        let results = run(
            &mut buf,
            &[Pattern::SolidBits],
            3,
            false,
            &mut sink,
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
        let mut sink = make_sink();
        let results = run(&mut buf, Pattern::ALL, 1, false, &mut sink, None, &|_| {}).unwrap();
        assert!(results.len() == 1);
        check!(results[0].pattern_results.len() == Pattern::ALL.len());
        check!(results[0].total_failures() == 0);
    }

    #[test]
    fn run_empty_patterns() {
        let mut buf = vec![0u64; 1024];
        let mut sink = make_sink();
        let results = run(&mut buf, &[], 1, false, &mut sink, None, &|_| {}).unwrap();
        check!(results.len() == 1);
        check!(results[0].pattern_results.is_empty());
        check!(results[0].total_failures() == 0);
    }

    #[test]
    fn run_parallel_clean() {
        let mut buf = vec![0u64; 4096];
        let mut sink = make_sink();
        let results = run(&mut buf, Pattern::ALL, 1, true, &mut sink, None, &|_| {}).unwrap();
        assert!(results.len() == 1);
        check!(results[0].total_failures() == 0);
    }

    /// Minimal resolver that always succeeds — for testing the resolver branch.
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
        let mut sink = make_sink();
        let results = run(
            &mut buf,
            &[Pattern::SolidBits],
            1,
            false,
            &mut sink,
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
