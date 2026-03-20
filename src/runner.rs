use std::time::Instant;

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use owo_colors::OwoColorize;

use crate::Failure;
use crate::alloc::LockedRegion;
use crate::pattern::{Pattern, run_pattern};

/// Result of running a single pattern.
pub struct PatternResult {
    pub pattern: Pattern,
    pub failures: Vec<Failure>,
    pub elapsed: std::time::Duration,
}

/// Result of a full pass (all patterns).
pub struct PassResult {
    pub pass_number: usize,
    pub pattern_results: Vec<PatternResult>,
}

impl PassResult {
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
pub fn run(
    region: &mut LockedRegion,
    patterns: &[Pattern],
    passes: usize,
    parallel: bool,
) -> Vec<PassResult> {
    let mp = MultiProgress::new();
    let pass_style =
        ProgressStyle::with_template("{prefix} [{bar:30.cyan/dim}] {pos}/{len} patterns  {msg}")
            .unwrap()
            .progress_chars("=> ");
    let sub_style =
        ProgressStyle::with_template("  {prefix:<20} [{bar:30.yellow/dim}] {pos}/{len}")
            .unwrap()
            .progress_chars("=> ");

    let size_mb = region.len() as f64 / (1024.0 * 1024.0);
    println!(
        "{} Testing {:.1} MiB across {} pass(es) with {} pattern(s){}\n",
        "ferrite".bold(),
        size_mb,
        passes,
        patterns.len(),
        if parallel { "" } else { "  (sequential)" },
    );

    let mut results = Vec::with_capacity(passes);

    for pass in 0..passes {
        let pass_pb = mp.add(ProgressBar::new(patterns.len() as u64));
        pass_pb.set_style(pass_style.clone());
        pass_pb.set_prefix(format!("Pass {}/{}", pass + 1, passes));

        let mut pattern_results = Vec::with_capacity(patterns.len());
        for &pattern in patterns {
            let sub_passes = pattern.sub_passes();

            // Show a sub-pass bar for patterns with more than one internal iteration.
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
            let start = Instant::now();
            let failures = run_pattern(pattern, buf, parallel, &mut || {
                if let Some(pb) = &inner_pb {
                    pb.inc(1);
                }
            });
            let elapsed = start.elapsed();

            if let Some(pb) = inner_pb {
                pb.finish_and_clear();
            }

            if failures.is_empty() {
                pass_pb.println(format!(
                    "  {} {:<20} {:>8.1}ms",
                    "PASS".green(),
                    pattern.to_string(),
                    elapsed.as_secs_f64() * 1000.0,
                ));
            } else {
                pass_pb.println(format!(
                    "  {} {:<20} {:>8.1}ms  ({} errors)",
                    "FAIL".red().bold(),
                    pattern.to_string(),
                    elapsed.as_secs_f64() * 1000.0,
                    failures.len(),
                ));
                for f in &failures {
                    pass_pb.println(format!("       {f}"));
                }
            }

            pattern_results.push(PatternResult {
                pattern,
                failures,
                elapsed,
            });
            pass_pb.inc(1);
        }
        pass_pb.finish_and_clear();

        let pass_result = PassResult {
            pass_number: pass + 1,
            pattern_results,
        };
        let total = pass_result.total_failures();
        if total == 0 {
            println!(
                "  Pass {}/{}: {}",
                pass + 1,
                passes,
                "all patterns passed".green(),
            );
        } else {
            println!(
                "  Pass {}/{}: {}",
                pass + 1,
                passes,
                format!("{total} total failure(s)").red().bold(),
            );
        }
        println!();
        results.push(pass_result);
    }

    results
}
