//! Coverage-store lifecycle glue.
//!
//! Opening the cross-run `--coverage-file` store, deriving the `--cull` hostage
//! set, merging a completed run into the store, and attaching cumulative +
//! gap-classification stats to run results.
//!
//! This is the glue between [`crate::physmem::coverage::CoverageStore`] and a
//! finished [`crate::runner::RunResults`]. It lives in the library (rather than
//! the binary) so its "does this run count toward coverage" branching is
//! reachable by tests.

use std::path::{Path, PathBuf};

use snafu::{OptionExt, ResultExt, Whatever, whatever};

use crate::physmem::coverage::CoverageStore;
use crate::physmem::pfn::PfnRange;
use crate::runner::RunResults;
use crate::units::UnitSystem;

/// A loaded (or freshly initialized) coverage store plus its file path.
pub struct CoverageCtx {
    pub store: CoverageStore,
    pub path: PathBuf,
}

impl CoverageCtx {
    /// Open the `--coverage-file` store when configured: load and validate an
    /// existing file (reporting cumulative coverage) or initialize a new store.
    ///
    /// Returns `Ok(None)` when no coverage file was requested.
    ///
    /// # Errors
    ///
    /// Fails when `--no-phys` is set (physical resolution is required), when the
    /// machine cannot be fingerprinted, or when an existing file is unreadable,
    /// invalid, or belongs to a different memory configuration.
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub fn open(coverage_file: Option<&Path>, no_phys: bool) -> Result<Option<Self>, Whatever> {
        let Some(path) = coverage_file else {
            return Ok(None);
        };
        if no_phys {
            whatever!("--coverage-file requires physical address resolution (remove --no-phys)");
        }
        let fingerprint = crate::physmem::sysmem::machine_fingerprint()
            .whatever_context("cannot fingerprint machine memory for coverage tracking")?;
        let loaded = CoverageStore::load(path, fingerprint).with_whatever_context(|_| {
            format!("failed to load coverage file: {}", path.display())
        })?;
        let store = if let Some(store) = loaded {
            let covered = store.covered_bytes();
            let installed = crate::physmem::sysmem::installed_ram().map_or(0, |r| r.bytes);
            let pct = if installed > 0 {
                covered as f64 / installed as f64 * 100.0
            } else {
                0.0
            };
            tracing::info!(
                "cumulative coverage: {} / {} ({pct:.1}%) across {} previous run(s)",
                crate::units::format_size(covered as usize),
                crate::units::format_size(installed as usize),
                store.runs.len(),
            );
            store
        } else {
            tracing::info!("starting new coverage file: {}", path.display());
            CoverageStore::new(fingerprint)
        };
        Ok(Some(Self {
            store,
            path: path.to_path_buf(),
        }))
    }
}

/// The covered set the `--cull` sieve should hold hostage, when culling is
/// requested. clap guarantees `--cull` implies `--coverage-file`.
#[must_use]
pub fn cull_ranges(cull: bool, ctx: Option<&CoverageCtx>) -> Option<Vec<PfnRange>> {
    cull.then(|| ctx.map(|c| c.store.ranges.clone()).unwrap_or_default())
}

/// Merge a completed run into the coverage store, persist it, and attach
/// cumulative stats to the results. Interrupted runs are not merged -- their
/// frames were not tested by every selected pattern.
///
/// Returns the covered set for gap classification: the store's cumulative
/// ranges when one is active, this run's frames otherwise. `None` when the
/// run cannot count toward coverage (unresolved or interrupted).
pub fn finalize_coverage(
    ctx: Option<CoverageCtx>,
    run_ranges: Option<Vec<PfnRange>>,
    results: &mut RunResults,
) -> Option<Vec<PfnRange>> {
    let Some(ranges) = run_ranges else {
        if ctx.is_some() {
            tracing::warn!("coverage store not updated: physical address resolution unavailable");
        }
        return None;
    };
    let interrupted = results
        .passes
        .iter()
        .flat_map(|p| &p.pattern_results)
        .any(|r| r.interrupted);
    if interrupted {
        if ctx.is_some() {
            tracing::warn!("coverage store not updated: run was interrupted");
        }
        return None;
    }
    let Some(mut ctx) = ctx else {
        return Some(ranges);
    };

    let patterns = results
        .config
        .patterns
        .iter()
        .map(ToString::to_string)
        .collect();
    let delta = ctx.store.record_run(
        &ranges,
        jiff::Timestamp::now(),
        patterns,
        results.config.passes,
        results.total_failures as u64,
    );
    if let Err(e) = ctx.store.save(&ctx.path) {
        tracing::warn!("failed to save coverage file: {e}");
    }
    results
        .coverage
        .attach_cumulative(crate::physmem::sysmem::Cumulative {
            new_bytes: delta.new_bytes,
            cumulative_bytes: delta.cumulative_bytes,
            runs: delta.runs,
        });
    Some(std::mem::take(&mut ctx.store.ranges))
}

/// Report the `--cull`-at-ceiling outcome.
///
/// Every acquirable frame is already covered, so no run happened and the
/// process exits successfully. Renders cumulative coverage plus the gap
/// classification for table output; JSON output stays empty (no run events
/// occurred) with the detail on stderr.
///
/// `render_table` is true only for human table output; JSON output skips the
/// rendered report (its stdout is a JSON-only surface).
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn report_cull_ceiling(
    ctx: Option<&CoverageCtx>,
    covered: &[PfnRange],
    render_table: bool,
    unit_system: UnitSystem,
) {
    tracing::info!(
        "--cull: nothing new to test; every acquirable frame is already covered on this boot"
    );
    if !render_table {
        return;
    }
    let gap = crate::physmem::gap::classify_system_gaps(covered);
    let installed = crate::physmem::sysmem::installed_ram().map_or(0, |r| r.bytes);
    let (cumulative, runs) = ctx.map_or((0, 0), |c| {
        (c.store.covered_bytes(), c.store.runs.len() as u64)
    });
    crate::results::render_ceiling_report(
        &mut std::io::stdout(),
        cumulative,
        installed,
        runs,
        gap,
        unit_system,
    )
    .unwrap_or_else(|e| eprintln!("warning: failed to render results: {e}"));
}

/// Classify what the untested remainder of installed RAM is doing and attach
/// the breakdown to the results. Requires root (`/proc/kpageflags`); silently
/// skipped otherwise.
pub fn attach_gap_classification(covered: Option<Vec<PfnRange>>, results: &mut RunResults) {
    if let Some(covered) = covered
        && let Some(report) = crate::physmem::gap::classify_system_gaps(&covered)
    {
        results.coverage.attach_gap(report);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::pattern::Pattern;
    use crate::physmem::coverage::{CoverageStore, fingerprint_from};
    use crate::physmem::pfn::{Pfn, PfnRange};
    use crate::physmem::sysmem::{Coverage, RamSource};
    use crate::runner::{PassResult, PatternResult, RunConfig, RunResults};

    use super::*;

    fn range(start: u64, count: u64) -> PfnRange {
        PfnRange {
            start: Pfn(start),
            count,
        }
    }

    fn results_with(interrupted: bool) -> RunResults {
        let pattern_result = PatternResult {
            pattern: Pattern::SolidBits,
            failures: vec![],
            elapsed: Duration::ZERO,
            bytes_processed: 0,
            interrupted,
            capped: false,
        };
        let pass = PassResult {
            pass_number: 1,
            pattern_results: vec![pattern_result],
            ecc_deltas: vec![],
        };
        let config = RunConfig {
            size: 4096,
            passes: 1,
            patterns: vec![Pattern::SolidBits],
            workers: 1,
        };
        RunResults::from_passes(vec![pass], config, Duration::ZERO)
    }

    fn measured() -> Coverage {
        Coverage::Measured {
            tested_bytes: 4096,
            total_bytes: 8192,
            source: RamSource::ProcIomem,
            cumulative: None,
            gap: None,
        }
    }

    fn store_ctx(dir: &std::path::Path) -> CoverageCtx {
        let fp = fingerprint_from(8192, &[(0x1000, 0xffff)]);
        CoverageCtx {
            store: CoverageStore::new(fp),
            path: dir.join("coverage.json"),
        }
    }

    mod cull_ranges {
        use assert2::check;

        use super::*;

        #[test]
        fn disabled_yields_none() {
            check!(super::super::cull_ranges(false, None).is_none());
        }

        #[test]
        fn enabled_without_store_yields_empty() {
            let ranges = super::super::cull_ranges(true, None);
            check!(ranges == Some(vec![]));
        }

        #[test]
        fn enabled_with_store_returns_covered_set() {
            let dir = tempfile::tempdir().unwrap();
            let mut ctx = store_ctx(dir.path());
            ctx.store.ranges = vec![range(5, 3)];
            let ranges = super::super::cull_ranges(true, Some(&ctx));
            check!(ranges == Some(vec![range(5, 3)]));
        }
    }

    mod finalize_coverage {
        use assert2::check;

        use super::*;

        #[test]
        fn no_run_ranges_returns_none() {
            let mut results = results_with(false);
            let covered = super::super::finalize_coverage(None, None, &mut results);
            check!(covered.is_none());
        }

        #[test]
        fn interrupted_run_does_not_count() {
            let mut results = results_with(true);
            let covered =
                super::super::finalize_coverage(None, Some(vec![range(1, 2)]), &mut results);
            check!(covered.is_none());
        }

        #[test]
        fn interrupted_run_with_store_does_not_record() {
            let dir = tempfile::tempdir().unwrap();
            let ctx = store_ctx(dir.path());
            let mut results = results_with(true);
            let covered =
                super::super::finalize_coverage(Some(ctx), Some(vec![range(1, 2)]), &mut results);
            check!(covered.is_none());
            // Store was not persisted: nothing to record for an interrupted run.
            check!(!dir.path().join("coverage.json").exists());
        }

        #[test]
        fn clean_run_without_store_returns_run_ranges() {
            let mut results = results_with(false);
            let covered =
                super::super::finalize_coverage(None, Some(vec![range(4, 6)]), &mut results);
            check!(covered == Some(vec![range(4, 6)]));
        }

        #[test]
        fn clean_run_with_store_records_and_attaches_cumulative() {
            let dir = tempfile::tempdir().unwrap();
            let ctx = store_ctx(dir.path());
            let mut results = results_with(false);
            results.coverage = measured();

            let covered =
                super::super::finalize_coverage(Some(ctx), Some(vec![range(10, 5)]), &mut results);

            // Returns the store's cumulative ranges (the merged set).
            check!(covered == Some(vec![range(10, 5)]));
            // Persisted to disk.
            check!(dir.path().join("coverage.json").exists());
            // Cumulative stats attached to the measured coverage.
            let Coverage::Measured { cumulative, .. } = results.coverage else {
                panic!("expected measured coverage");
            };
            let cumulative = cumulative.expect("cumulative stats attached");
            check!(cumulative.runs == 1);
            check!(cumulative.new_bytes == 5 * crate::physmem::PAGE_BYTES);
        }

        #[test]
        fn save_failure_still_returns_ranges() {
            // A path whose parent does not exist makes save() fail; the run
            // still counts and the covered set is returned.
            let ctx = CoverageCtx {
                store: CoverageStore::new(fingerprint_from(8192, &[(0x1000, 0xffff)])),
                path: std::path::PathBuf::from("/nonexistent-ferrite-dir/coverage.json"),
            };
            let mut results = results_with(false);
            let covered =
                super::super::finalize_coverage(Some(ctx), Some(vec![range(2, 2)]), &mut results);
            check!(covered == Some(vec![range(2, 2)]));
        }
    }

    mod attach_gap {
        use assert2::check;

        use super::*;

        #[test]
        fn none_covered_leaves_coverage_untouched() {
            let mut results = results_with(false);
            results.coverage = measured();
            super::super::attach_gap_classification(None, &mut results);
            let Coverage::Measured { gap, .. } = results.coverage else {
                panic!("expected measured coverage");
            };
            check!(gap.is_none());
        }

        #[test]
        fn some_covered_without_root_leaves_gap_unset() {
            // classify_system_gaps needs /proc/kpageflags (root); in the test
            // environment it yields None, so no gap is attached.
            let mut results = results_with(false);
            results.coverage = measured();
            super::super::attach_gap_classification(Some(vec![range(0, 1)]), &mut results);
            let Coverage::Measured { .. } = results.coverage else {
                panic!("expected measured coverage");
            };
        }
    }
}
