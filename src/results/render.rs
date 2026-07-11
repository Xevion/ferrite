use std::io::{self, Write};

use owo_colors::OwoColorize;

use super::ResultsDoc;
use crate::units::UnitSystem;

/// Write the per-pattern `PASS`/`FAIL`/`INTR` result line shared between the
/// live headless printer ([`crate::headless::HeadlessPrinter`]) and the full
/// post-run table renderer ([`TableRenderer`]).
///
/// Does not print individual failure details -- callers with access to raw
/// `Failure` values append those separately.
pub fn write_pattern_result_line(
    out: &mut dyn Write,
    name: &str,
    elapsed_ms: f64,
    throughput: crate::units::Rate,
    failure_count: u64,
    interrupted: bool,
    capped: bool,
) -> io::Result<()> {
    let suffix = if interrupted { "  (interrupted)" } else { "" };
    let elapsed = crate::units::format_millis(elapsed_ms);
    if failure_count == 0 {
        let label = if interrupted {
            "INTR".yellow().bold().to_string()
        } else {
            "PASS".green().to_string()
        };
        writeln!(
            out,
            "  {label} {name:<20} {elapsed:>10}  {throughput:>}{suffix}",
        )
    } else {
        // When capped, more failures existed than were collected -- show `N+`
        // and flag the cap so the count isn't mistaken for the true total.
        let count = if capped {
            format!(
                "{}+ failures, capped at --max-errors",
                crate::units::format_count(failure_count)
            )
        } else {
            format!("{} failures", crate::units::format_count(failure_count))
        };
        writeln!(
            out,
            "  {} {name:<20} {elapsed:>10}  {throughput:>}  ({count}){suffix}",
            "FAIL".red().bold(),
        )
    }
}

/// Write the per-pass summary line (`Pass N/M: ...`) plus its trailing blank
/// line, shared between the live headless printer and the full post-run
/// table renderer.
pub fn write_pass_summary_line(
    out: &mut dyn Write,
    pass_number: u64,
    total_passes: u64,
    failures: u64,
) -> io::Result<()> {
    if failures == 0 {
        writeln!(
            out,
            "  Pass {pass_number}/{total_passes}: {}",
            "all patterns passed".green(),
        )?;
    } else {
        writeln!(
            out,
            "  Pass {pass_number}/{total_passes}: {}",
            format!("{} total failure(s)", crate::units::format_count(failures))
                .red()
                .bold(),
        )?;
    }
    writeln!(out)
}

/// Write the final PASS/FAIL verdict line, optionally suffixed with a
/// parenthesized annotation (e.g. total elapsed time). The live headless
/// printer passes `None` since it reports elapsed time via other events;
/// the full post-run table renderer passes the run's elapsed display.
pub fn write_verdict_line(
    out: &mut dyn Write,
    total_failures: u64,
    suffix: Option<&str>,
) -> io::Result<()> {
    let suffix = suffix.map_or_else(String::new, |s| format!(" ({s})"));
    if total_failures == 0 {
        writeln!(out, "{}{suffix}", "All tests passed.".green().bold())
    } else {
        writeln!(
            out,
            "{}{suffix}",
            format!(
                "{} failure(s) detected.",
                crate::units::format_count(total_failures)
            )
            .red()
            .bold(),
        )
    }
}

/// Trait for rendering a `ResultsDoc` to a writer.
pub trait ResultsRenderer {
    /// Render the results document to the given output.
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] if writing fails.
    fn render(&self, doc: &ResultsDoc, out: &mut dyn Write) -> io::Result<()>;
}

/// Human-readable summary table with error detail and color support.
///
/// When `full` is true, includes per-pass/per-pattern result lines
/// (suitable for post-TUI rendering where no live output was shown).
/// When false, only renders the error analysis and final verdict
/// (suitable after `HeadlessPrinter` already streamed live results).
pub struct TableRenderer {
    unit_system: UnitSystem,
    full: bool,
}

impl TableRenderer {
    /// Summary-only renderer (after live headless output).
    #[must_use]
    pub const fn new(unit_system: UnitSystem) -> Self {
        Self {
            unit_system,
            full: false,
        }
    }

    /// Full renderer including per-pattern results (after TUI exit).
    #[must_use]
    pub const fn full(unit_system: UnitSystem) -> Self {
        Self {
            unit_system,
            full: true,
        }
    }

    /// Render the physical coverage block, including cross-run cumulative
    /// stats when a coverage store contributed them.
    fn render_coverage(&self, cov: &super::CoverageDoc<'_>, out: &mut dyn Write) -> io::Result<()> {
        writeln!(out)?;
        writeln!(out, "Physical coverage")?;
        if !cov.is_measured() {
            return writeln!(out, "  unavailable (no physical address resolution)");
        }
        let tested = crate::units::Size::new(cov.tested_bytes() as f64, self.unit_system);
        let total = crate::units::Size::new(cov.total_bytes() as f64, self.unit_system);
        writeln!(out, "  Tested:    {tested}")?;
        writeln!(out, "  Installed: {total}  ({})", cov.source_label())?;
        writeln!(out, "  Coverage:  {}", format_percent(cov.percent()))?;
        if let Some(cum) = cov.cumulative() {
            let new = crate::units::Size::new(cum.new_bytes() as f64, self.unit_system);
            let cum_size = crate::units::Size::new(cum.cumulative_bytes() as f64, self.unit_system);
            writeln!(out, "  New:       {new} this run")?;
            writeln!(
                out,
                "  Cumulative: {cum_size} ({}) across {} run(s)",
                format_percent(cum.percent()),
                cum.runs(),
            )?;
        }
        if let Some(gap) = cov.gap() {
            let report = crate::physmem::gap::GapReport {
                free_bytes: gap.free_bytes(),
                reclaimable_bytes: gap.reclaimable_bytes(),
                in_use_bytes: gap.in_use_bytes(),
                unreachable_bytes: gap.unreachable_bytes(),
                unknown_bytes: gap.unknown_bytes(),
            };
            write_gap_line(out, &report, self.unit_system)?;
        }
        Ok(())
    }
}

/// Format a coverage percentage, scaling precision so small runs still show a
/// nonzero figure instead of collapsing to `0.0%`.
fn format_percent(pct: f64) -> String {
    if pct >= 1.0 {
        format!("{pct:.1}%")
    } else if pct >= 0.01 {
        format!("{pct:.2}%")
    } else {
        format!("{pct:.3}%")
    }
}

/// Write the `Untested:` line breaking the gap down by frame class.
fn write_gap_line(
    out: &mut dyn Write,
    gap: &crate::physmem::gap::GapReport,
    unit_system: crate::units::UnitSystem,
) -> io::Result<()> {
    use std::fmt::Write as _;

    let size = |b: u64| crate::units::Size::new(b as f64, unit_system);
    let mut breakdown = format!(
        "{} free + {} reclaimable + {} in-use + {} unreachable",
        size(gap.free_bytes),
        size(gap.reclaimable_bytes),
        size(gap.in_use_bytes),
        size(gap.unreachable_bytes),
    );
    if gap.unknown_bytes > 0 {
        let _ = write!(breakdown, " + {} unknown", size(gap.unknown_bytes));
    }
    writeln!(
        out,
        "  Untested:  {} = {breakdown}",
        size(gap.total_bytes())
    )
}

/// Render the `--cull`-at-ceiling report.
///
/// The sieve held every acquirable frame hostage, so no run happened. Shows
/// cumulative coverage and the untested-remainder classification so the
/// ceiling reads as *done for this boot*, not as a failure.
///
/// # Errors
///
/// Propagates I/O errors from the writer.
pub fn render_ceiling_report(
    out: &mut dyn Write,
    cumulative_bytes: u64,
    installed_bytes: u64,
    runs: u64,
    gap: Option<crate::physmem::gap::GapReport>,
    unit_system: crate::units::UnitSystem,
) -> io::Result<()> {
    let size = |b: u64| crate::units::Size::new(b as f64, unit_system);
    let pct = if installed_bytes == 0 {
        0.0
    } else {
        cumulative_bytes as f64 / installed_bytes as f64 * 100.0
    };
    writeln!(
        out,
        "Nothing new to test: every acquirable frame is already covered."
    )?;
    writeln!(out)?;
    writeln!(out, "Physical coverage")?;
    writeln!(
        out,
        "  Cumulative: {} ({}) across {runs} run(s)",
        size(cumulative_bytes),
        format_percent(pct),
    )?;
    if let Some(gap) = gap {
        write_gap_line(out, &gap, unit_system)?;
    }
    writeln!(out)?;
    writeln!(
        out,
        "Coverage is at its ceiling for this boot; reboot to reshuffle occupancy \
         and reach untested frames."
    )
}

impl ResultsRenderer for TableRenderer {
    fn render(&self, doc: &ResultsDoc, out: &mut dyn Write) -> io::Result<()> {
        let total_failures = doc.total_failures();
        let elapsed_ms = doc.elapsed_ms();

        // Per-pass, per-pattern results (only in full mode)
        if self.full {
            let total_passes = doc.config().passes();
            for pass in doc.passes() {
                for p in pass.pattern_results() {
                    let failures = p.failure_count();
                    let elapsed_ms = p.elapsed_ms();
                    let throughput = crate::units::Rate::new(
                        p.bytes_processed() as f64 / (elapsed_ms / 1000.0),
                        self.unit_system,
                    );
                    // An interrupted pattern is incomplete: a clean result
                    // can't be trusted as a PASS, so flag it distinctly.
                    let interrupted = p.interrupted();
                    write_pattern_result_line(
                        out,
                        p.pattern_name(),
                        elapsed_ms,
                        throughput,
                        failures,
                        interrupted,
                        p.capped(),
                    )?;
                }
                let pass_number = pass.pass_number();
                let pass_failures = pass.total_failures();
                write_pass_summary_line(out, pass_number, total_passes, pass_failures)?;
            }
        }

        // Error analysis block
        if let Some(ea) = doc.error_analysis() {
            writeln!(out)?;
            writeln!(out, "  Error analysis: {}", ea.classification_str())?;
            writeln!(out, "  Affected bits: 0x{:016x}", ea.union_xor_mask())?;

            let bit_positions = ea.bit_positions();
            if !bit_positions.is_empty() {
                let bp_str: Vec<String> = bit_positions
                    .iter()
                    .map(|(pos, count)| {
                        format!(
                            "bit {pos} ({}x)",
                            crate::units::format_count((*count).into())
                        )
                    })
                    .collect();
                writeln!(out, "  Bit flip counts: {}", bp_str.join(", "))?;
            }

            if let (Some(lo), Some(hi)) = (ea.lowest_phys(), ea.highest_phys()) {
                writeln!(out, "  Physical address range: 0x{lo:x} -- 0x{hi:x}")?;
            }

            let per_pattern = ea.per_pattern_failures();
            if !per_pattern.is_empty() {
                let pp_str: Vec<String> = per_pattern
                    .iter()
                    .map(|(name, count)| format!("{name}: {}", crate::units::format_count(*count)))
                    .collect();
                writeln!(out, "  Per-pattern failures: {}", pp_str.join(", "))?;
            }
        }

        // Physical coverage block -- the memory-centric headline for the run.
        if let Some(cov) = doc.coverage() {
            self.render_coverage(&cov, out)?;
        }

        // Final verdict
        writeln!(out)?;
        let elapsed_display = crate::units::format_millis(elapsed_ms);
        write_verdict_line(out, total_failures, Some(&elapsed_display))
    }
}

/// Pretty-printed JSON output of the full results document.
pub struct JsonRenderer;

impl ResultsRenderer for JsonRenderer {
    fn render(&self, doc: &ResultsDoc, out: &mut dyn Write) -> io::Result<()> {
        serde_json::to_writer_pretty(&mut *out, doc.as_value()).map_err(io::Error::other)?;
        writeln!(out)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::super::fixtures::{
        clean_results, covered_results, failing_results, make_config, multi_pattern_results,
    };
    use super::*;
    use crate::pattern::Pattern;
    use crate::runner::{PassResult, PatternResult, RunResults};

    mod format_percent_tests {
        use assert2::check;

        use super::*;

        #[test]
        fn scales_precision_by_magnitude() {
            check!(format_percent(42.567) == "42.6%");
            check!(format_percent(0.1953) == "0.20%");
            check!(format_percent(0.004) == "0.004%");
        }
    }

    mod pattern_result_line {
        use assert2::check;

        use super::*;
        use crate::units::Rate;

        fn line(failure_count: u64, interrupted: bool, capped: bool) -> String {
            let mut out = Vec::new();
            let rate = Rate::new(1.0e9, UnitSystem::Binary);
            write_pattern_result_line(
                &mut out,
                "Solid Bits",
                12.3,
                rate,
                failure_count,
                interrupted,
                capped,
            )
            .unwrap();
            String::from_utf8(out).unwrap()
        }

        #[test]
        fn capped_line_flags_truncation() {
            let s = line(1000, false, true);
            check!(s.contains("FAIL"));
            check!(s.contains("1,000+ failures"));
            check!(s.contains("capped"));
        }

        #[test]
        fn uncapped_line_shows_plain_count() {
            let s = line(3, false, false);
            check!(s.contains("3 failures"));
            check!(!s.contains("capped"));
        }

        #[test]
        fn clean_line_has_no_cap_annotation() {
            // capped is meaningless with zero failures; the PASS line ignores it.
            let s = line(0, false, true);
            check!(s.contains("PASS"));
            check!(!s.contains("capped"));
        }
    }

    mod table_renderer {
        use assert2::assert;

        use super::*;

        fn render_to_string(results: &RunResults) -> String {
            let doc = ResultsDoc::from_results(results);
            let renderer = TableRenderer::new(UnitSystem::Binary);
            let mut buf = Vec::new();
            renderer.render(&doc, &mut buf).unwrap();
            String::from_utf8(buf).unwrap()
        }

        fn render_full_to_string(results: &RunResults) -> String {
            let doc = ResultsDoc::from_results(results);
            let renderer = TableRenderer::full(UnitSystem::Binary);
            let mut buf = Vec::new();
            renderer.render(&doc, &mut buf).unwrap();
            String::from_utf8(buf).unwrap()
        }

        #[test]
        fn full_renders_one_pass_block_per_pass() {
            let out = render_full_to_string(&multi_pattern_results());
            assert!(out.matches("Pass 1/1").count() == 1);
            assert!(out.matches("SolidBits").count() == 1);
            assert!(out.matches("Checkerboard").count() == 1);
        }

        #[test]
        fn full_renderer_flags_interrupted() {
            let results = RunResults::from_passes(
                vec![PassResult {
                    pass_number: 1,
                    pattern_results: vec![PatternResult {
                        pattern: Pattern::SolidBits,
                        failures: vec![],
                        elapsed: Duration::from_millis(10),
                        bytes_processed: 1024,
                        interrupted: true,
                        capped: false,
                    }],
                    ecc_deltas: vec![],
                }],
                make_config(),
                Duration::from_millis(10),
            );
            let out = render_full_to_string(&results);
            assert!(out.contains("INTR"));
            assert!(out.contains("(interrupted)"));
            assert!(!out.contains("PASS"));
        }

        #[test]
        fn clean_run_shows_passed() {
            let out = render_to_string(&clean_results());
            assert!(out.contains("All tests passed"));
        }

        #[test]
        fn renders_measured_coverage_block() {
            let out = render_to_string(&covered_results());
            assert!(out.contains("Physical coverage"));
            assert!(out.contains("Tested:"));
            assert!(out.contains("64.0 MiB"));
            assert!(out.contains("Installed:"));
            assert!(out.contains("32.0 GiB"));
            assert!(out.contains("/proc/iomem"));
            assert!(out.contains("Coverage:"));
            assert!(out.contains("0.20%"));
        }

        #[test]
        fn renders_unavailable_coverage_block() {
            // clean_results() defaults coverage to Unavailable.
            let out = render_to_string(&clean_results());
            assert!(out.contains("Physical coverage"));
            assert!(out.contains("unavailable"));
        }

        #[test]
        fn renders_cumulative_lines_when_store_active() {
            let mut r = covered_results();
            r.coverage
                .attach_cumulative(crate::physmem::sysmem::Cumulative {
                    new_bytes: 32 * 1024 * 1024,
                    cumulative_bytes: 16 * 1024 * 1024 * 1024,
                    runs: 3,
                });
            let out = render_to_string(&r);
            assert!(out.contains("New:       32.0 MiB this run"));
            assert!(out.contains("Cumulative: 16.0 GiB (50.0%) across 3 run(s)"));
        }

        #[test]
        fn no_cumulative_lines_without_store() {
            let out = render_to_string(&covered_results());
            assert!(!out.contains("Cumulative:"));
            assert!(!out.contains("New:"));
        }

        #[test]
        fn renders_gap_breakdown_when_classified() {
            let mut r = covered_results();
            r.coverage.attach_gap(crate::physmem::gap::GapReport {
                free_bytes: 2 * 1024 * 1024 * 1024,
                reclaimable_bytes: 1024 * 1024 * 1024,
                in_use_bytes: 512 * 1024 * 1024,
                unreachable_bytes: 256 * 1024 * 1024,
                unknown_bytes: 0,
            });
            let out = render_to_string(&r);
            assert!(out.contains("Untested:"));
            assert!(out.contains("3.75 GiB ="));
            assert!(out.contains("2.00 GiB free"));
            assert!(out.contains("1.00 GiB reclaimable"));
            assert!(out.contains("512 MiB in-use"));
            assert!(out.contains("256 MiB unreachable"));
            assert!(!out.contains("unknown"));
        }

        #[test]
        fn gap_breakdown_includes_unknown_when_nonzero() {
            let mut r = covered_results();
            r.coverage.attach_gap(crate::physmem::gap::GapReport {
                unknown_bytes: 4096,
                ..crate::physmem::gap::GapReport::default()
            });
            let out = render_to_string(&r);
            assert!(out.contains("4.00 KiB unknown"));
        }

        #[test]
        fn no_gap_breakdown_without_classification() {
            let out = render_to_string(&covered_results());
            assert!(!out.contains("Untested:"));
        }

        #[test]
        fn failing_run_shows_error_analysis() {
            let out = render_to_string(&failing_results());
            assert!(out.contains("failure(s) detected"));
            assert!(out.contains("Error analysis"));
            assert!(out.contains("stuck bit"));
            assert!(out.contains("Affected bits"));
            assert!(out.contains("Physical address range"));
            assert!(out.contains("0x5000"));
            assert!(out.contains("0x9000"));
        }

        #[test]
        fn shows_per_pattern_failures() {
            let out = render_to_string(&failing_results());
            assert!(out.contains("Per-pattern failures"));
            assert!(out.contains("SolidBits"));
        }

        #[test]
        fn shows_bit_flip_counts() {
            let out = render_to_string(&failing_results());
            assert!(out.contains("Bit flip counts"));
            assert!(out.contains("bit 20"));
        }

        #[test]
        fn shows_elapsed_time() {
            let out = render_to_string(&clean_results());
            // 100ms < 1s so should render in milliseconds.
            assert!(out.contains("100.0 ms"));
        }

        #[test]
        fn long_elapsed_shows_seconds() {
            let results = RunResults::from_passes(
                vec![PassResult {
                    pass_number: 1,
                    pattern_results: vec![],
                    ecc_deltas: vec![],
                }],
                make_config(),
                Duration::from_secs(5),
            );
            let doc = ResultsDoc::from_results(&results);
            let renderer = TableRenderer::new(UnitSystem::Binary);
            let mut buf = Vec::new();
            renderer.render(&doc, &mut buf).unwrap();
            let out = String::from_utf8(buf).unwrap();
            assert!(out.contains("5.00 s"));
        }
    }

    mod ceiling_report {
        use assert2::assert;

        use crate::physmem::gap::GapReport;
        use crate::results::render_ceiling_report;
        use crate::units::UnitSystem;

        const GIB: u64 = 1024 * 1024 * 1024;

        fn render(cumulative: u64, installed: u64, runs: u64, gap: Option<GapReport>) -> String {
            let mut buf = Vec::new();
            render_ceiling_report(
                &mut buf,
                cumulative,
                installed,
                runs,
                gap,
                UnitSystem::Binary,
            )
            .unwrap();
            String::from_utf8(buf).unwrap()
        }

        #[test]
        fn reports_cumulative_and_reboot_hint() {
            let out = render(16 * GIB, 32 * GIB, 8, None);
            assert!(out.contains("Nothing new to test"));
            assert!(out.contains("Cumulative: 16.0 GiB (50.0%) across 8 run(s)"));
            assert!(out.contains("reboot"));
        }

        #[test]
        fn includes_gap_breakdown_when_classified() {
            let gap = GapReport {
                free_bytes: GIB,
                reclaimable_bytes: GIB / 2,
                in_use_bytes: GIB / 4,
                unreachable_bytes: GIB / 4,
                unknown_bytes: 0,
            };
            let out = render(30 * GIB, 32 * GIB, 8, Some(gap));
            assert!(out.contains("Untested:  2.00 GiB ="));
            assert!(out.contains("1.00 GiB free"));
            assert!(out.contains("512 MiB reclaimable"));
            assert!(out.contains("256 MiB in-use"));
            assert!(out.contains("256 MiB unreachable"));
            assert!(!out.contains("unknown"));
        }

        #[test]
        fn omits_gap_line_without_classification() {
            let out = render(16 * GIB, 32 * GIB, 1, None);
            assert!(!out.contains("Untested:"));
        }

        #[test]
        fn zero_installed_reports_zero_percent() {
            let out = render(16 * GIB, 0, 1, None);
            assert!(out.contains("(0.000%)"));
        }
    }

    mod json_renderer {
        use assert2::{assert, check};

        use super::*;

        #[test]
        fn produces_valid_json() {
            let doc = ResultsDoc::from_results(&clean_results());
            let mut buf = Vec::new();
            JsonRenderer.render(&doc, &mut buf).unwrap();
            let output = String::from_utf8(buf).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
            check!(parsed["total_failures"] == 0);
            assert!(parsed["config"].is_object());
        }

        #[test]
        fn includes_error_analysis_when_present() {
            let doc = ResultsDoc::from_results(&failing_results());
            let mut buf = Vec::new();
            JsonRenderer.render(&doc, &mut buf).unwrap();
            let output = String::from_utf8(buf).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
            assert!(parsed["error_analysis"].is_object());
            check!(parsed["total_failures"] == 2);
        }

        #[test]
        fn trailing_newline() {
            let doc = ResultsDoc::from_results(&clean_results());
            let mut buf = Vec::new();
            JsonRenderer.render(&doc, &mut buf).unwrap();
            assert!(buf.ends_with(b"\n"));
        }

        #[test]
        fn includes_coverage_status() {
            let doc = ResultsDoc::from_results(&clean_results());
            let mut buf = Vec::new();
            JsonRenderer.render(&doc, &mut buf).unwrap();
            let parsed: serde_json::Value =
                serde_json::from_str(&String::from_utf8(buf).unwrap()).unwrap();
            check!(parsed["coverage"]["status"] == "unavailable");
        }
    }
}
