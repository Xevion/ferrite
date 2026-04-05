use std::time::Instant;

use indicatif::{ProgressBar, ProgressStyle};

use crate::Failure;
use crate::alloc::LockedRegion;
use crate::edac::{EccDelta, EdacSnapshot};
use crate::output::OutputSink;
use crate::pattern::{Pattern, run_pattern};
use crate::phys::PhysResolver;

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
/// # Panics
///
/// Panics if progress bar template formatting fails (indicates a bug in the
/// hardcoded template string).
pub fn run(
    region: &mut LockedRegion,
    patterns: &[Pattern],
    passes: usize,
    parallel: bool,
    sink: &mut OutputSink,
    resolver: Option<&dyn PhysResolver>,
    on_activity: &(dyn Fn(f64) + Sync),
) -> Vec<PassResult> {
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

    sink.print_banner(region.len(), passes, patterns.len(), parallel);

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

            let buf = region.as_u64_slice_mut();
            let buf_bytes = (buf.len() as u64) * 8;
            let start = Instant::now();

            let mut sub_pass_count: u64 = 0;
            let mut failures = run_pattern(
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
            );
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

    results
}
