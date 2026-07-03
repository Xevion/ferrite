use std::io::{self, Write};
use std::path::Path;

use owo_colors::OwoColorize;

use crate::units::UnitSystem;

/// Typed wrapper over the JSON representation of [`crate::runner::RunResults`].
///
/// JSON is the canonical intermediate format. A `ResultsDoc` can be constructed
/// from a live `RunResults` or from a saved JSON file, enabling re-rendering
/// without re-running tests.
pub struct ResultsDoc(serde_json::Value);

impl ResultsDoc {
    /// Build from a live `RunResults` by serializing to JSON.
    ///
    /// # Panics
    ///
    /// Panics if `RunResults` fails to serialize (should never happen with
    /// well-formed data).
    #[must_use]
    pub fn from_results(results: &crate::runner::RunResults) -> Self {
        Self(serde_json::to_value(results).expect("RunResults must be serializable"))
    }

    /// Build from an already-parsed JSON value.
    #[must_use]
    pub fn from_json(value: serde_json::Value) -> Self {
        Self(value)
    }

    /// Load from a JSON file on disk.
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] if the file cannot be read or parsed.
    pub fn from_file(path: &Path) -> io::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let value: serde_json::Value = serde_json::from_str(&content)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(Self(value))
    }

    /// Total failure count across all passes.
    #[must_use]
    pub fn total_failures(&self) -> u64 {
        self.0["total_failures"].as_u64().unwrap_or(0)
    }

    /// Total elapsed time in milliseconds.
    #[must_use]
    pub fn elapsed_ms(&self) -> f64 {
        self.0["elapsed"].as_f64().unwrap_or(0.0)
    }

    /// Iterator over pass documents.
    pub fn passes(&self) -> impl Iterator<Item = PassDoc<'_>> {
        self.0["passes"]
            .as_array()
            .map(|a| a.iter().map(PassDoc).collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
    }

    /// Error analysis, if present.
    #[must_use]
    pub fn error_analysis(&self) -> Option<ErrorAnalysisDoc<'_>> {
        self.0["error_analysis"]
            .as_object()
            .map(|_| ErrorAnalysisDoc(&self.0["error_analysis"]))
    }

    /// Run configuration.
    #[must_use]
    pub fn config(&self) -> ConfigDoc<'_> {
        ConfigDoc(&self.0["config"])
    }

    /// Physical coverage, if the results carry a coverage object.
    #[must_use]
    pub fn coverage(&self) -> Option<CoverageDoc<'_>> {
        self.0["coverage"]
            .as_object()
            .map(|_| CoverageDoc(&self.0["coverage"]))
    }

    /// Access the raw inner JSON value.
    #[must_use]
    pub fn as_value(&self) -> &serde_json::Value {
        &self.0
    }
}

/// Borrowed view into a single pass within the results.
pub struct PassDoc<'a>(&'a serde_json::Value);

impl<'a> PassDoc<'a> {
    #[must_use]
    pub fn pass_number(&self) -> u64 {
        self.0["pass_number"].as_u64().unwrap_or(0)
    }

    #[must_use]
    pub fn total_failures(&self) -> u64 {
        self.pattern_results().map(|pr| pr.failure_count()).sum()
    }

    /// Iterator over pattern result documents in this pass.
    pub fn pattern_results(&self) -> impl Iterator<Item = PatternDoc<'a>> {
        self.0["pattern_results"]
            .as_array()
            .map(|a| a.iter().map(PatternDoc).collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
    }
}

/// Borrowed view into a single pattern result.
pub struct PatternDoc<'a>(&'a serde_json::Value);

impl PatternDoc<'_> {
    #[must_use]
    pub fn pattern_name(&self) -> &str {
        self.0["pattern"].as_str().unwrap_or("unknown")
    }

    #[must_use]
    pub fn elapsed_ms(&self) -> f64 {
        self.0["elapsed"].as_f64().unwrap_or(0.0)
    }

    #[must_use]
    pub fn bytes_processed(&self) -> u64 {
        self.0["bytes_processed"].as_u64().unwrap_or(0)
    }

    #[must_use]
    pub fn failure_count(&self) -> u64 {
        self.0["failures"].as_array().map_or(0, |a| a.len() as u64)
    }

    #[must_use]
    pub fn interrupted(&self) -> bool {
        self.0["interrupted"].as_bool().unwrap_or(false)
    }
}

/// Borrowed view into run configuration.
pub struct ConfigDoc<'a>(&'a serde_json::Value);

impl ConfigDoc<'_> {
    #[must_use]
    pub fn size(&self) -> u64 {
        self.0["size"].as_u64().unwrap_or(0)
    }

    #[must_use]
    pub fn passes(&self) -> u64 {
        self.0["passes"].as_u64().unwrap_or(0)
    }

    #[must_use]
    pub fn workers(&self) -> u64 {
        self.0["workers"].as_u64().unwrap_or(1)
    }
}

/// Borrowed view into physical coverage.
pub struct CoverageDoc<'a>(&'a serde_json::Value);

impl CoverageDoc<'_> {
    #[must_use]
    pub fn is_measured(&self) -> bool {
        self.0["status"].as_str() == Some("measured")
    }

    #[must_use]
    pub fn tested_bytes(&self) -> u64 {
        self.0["tested_bytes"].as_u64().unwrap_or(0)
    }

    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.0["total_bytes"].as_u64().unwrap_or(0)
    }

    /// Human-readable denominator source label.
    #[must_use]
    pub fn source_label(&self) -> &'static str {
        match self.0["source"].as_str() {
            Some("proc_iomem") => "/proc/iomem",
            _ => "MemTotal estimate",
        }
    }

    /// Tested fraction as a percentage; 0.0 when the denominator is zero.
    #[must_use]
    pub fn percent(&self) -> f64 {
        let total = self.total_bytes();
        if total == 0 {
            0.0
        } else {
            self.tested_bytes() as f64 / total as f64 * 100.0
        }
    }

    /// Cross-run cumulative stats, when a coverage store was active.
    #[must_use]
    pub fn cumulative(&self) -> Option<CumulativeDoc<'_>> {
        self.0["cumulative"]
            .as_object()
            .map(|_| CumulativeDoc(&self.0["cumulative"], self.total_bytes()))
    }

    /// Classification of the untested remainder, when the kpageflags scan ran.
    #[must_use]
    pub fn gap(&self) -> Option<GapDoc<'_>> {
        self.0["gap"].as_object().map(|_| GapDoc(&self.0["gap"]))
    }
}

/// Borrowed view into the untested-remainder classification.
pub struct GapDoc<'a>(&'a serde_json::Value);

impl GapDoc<'_> {
    #[must_use]
    pub fn free_bytes(&self) -> u64 {
        self.0["free_bytes"].as_u64().unwrap_or(0)
    }

    #[must_use]
    pub fn reclaimable_bytes(&self) -> u64 {
        self.0["reclaimable_bytes"].as_u64().unwrap_or(0)
    }

    #[must_use]
    pub fn in_use_bytes(&self) -> u64 {
        self.0["in_use_bytes"].as_u64().unwrap_or(0)
    }

    #[must_use]
    pub fn unreachable_bytes(&self) -> u64 {
        self.0["unreachable_bytes"].as_u64().unwrap_or(0)
    }

    #[must_use]
    pub fn unknown_bytes(&self) -> u64 {
        self.0["unknown_bytes"].as_u64().unwrap_or(0)
    }

    /// Total untested bytes across all classes.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.free_bytes()
            + self.reclaimable_bytes()
            + self.in_use_bytes()
            + self.unreachable_bytes()
            + self.unknown_bytes()
    }
}

/// Borrowed view into cross-run cumulative coverage stats.
pub struct CumulativeDoc<'a>(&'a serde_json::Value, u64);

impl CumulativeDoc<'_> {
    #[must_use]
    pub fn new_bytes(&self) -> u64 {
        self.0["new_bytes"].as_u64().unwrap_or(0)
    }

    #[must_use]
    pub fn cumulative_bytes(&self) -> u64 {
        self.0["cumulative_bytes"].as_u64().unwrap_or(0)
    }

    #[must_use]
    pub fn runs(&self) -> u64 {
        self.0["runs"].as_u64().unwrap_or(0)
    }

    /// Cumulative fraction of installed RAM, against the parent coverage
    /// denominator; 0.0 when that denominator is zero.
    #[must_use]
    pub fn percent(&self) -> f64 {
        if self.1 == 0 {
            0.0
        } else {
            self.cumulative_bytes() as f64 / self.1 as f64 * 100.0
        }
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

/// Borrowed view into error analysis.
pub struct ErrorAnalysisDoc<'a>(&'a serde_json::Value);

impl ErrorAnalysisDoc<'_> {
    /// Human-readable classification string.
    #[must_use]
    pub fn classification_str(&self) -> String {
        let c = &self.0["classification"];
        if let Some(obj) = c.as_object()
            && let Some(positions) = obj.get("StuckBit").and_then(|v| v["positions"].as_array())
        {
            let pos_str: Vec<String> = positions
                .iter()
                .filter_map(serde_json::Value::as_u64)
                .map(|p| format!("bit {p}"))
                .collect();
            return format!("stuck bit(s): {}", pos_str.join(", "));
        }
        if let Some(s) = c.as_str() {
            return match s {
                "Coupling" => "coupling/disturbance errors".to_owned(),
                "Mixed" => "mixed (stuck + coupling)".to_owned(),
                "NoErrors" => "no errors".to_owned(),
                other => other.to_owned(),
            };
        }
        "unknown".to_owned()
    }

    /// OR of all XOR masks.
    #[must_use]
    pub fn union_xor_mask(&self) -> u64 {
        self.0["union_xor_mask"].as_u64().unwrap_or(0)
    }

    /// Lowest physical address with an error.
    #[must_use]
    pub fn lowest_phys(&self) -> Option<u64> {
        self.0["lowest_phys"].as_u64()
    }

    /// Highest physical address with an error.
    #[must_use]
    pub fn highest_phys(&self) -> Option<u64> {
        self.0["highest_phys"].as_u64()
    }

    /// Per-pattern failure counts: `(pattern_name, count)`.
    #[must_use]
    pub fn per_pattern_failures(&self) -> Vec<(String, u64)> {
        self.0["per_pattern_failures"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|entry| {
                        let arr = entry.as_array()?;
                        let name = arr.first()?.as_str()?.to_owned();
                        let count = arr.get(1)?.as_u64()?;
                        Some((name, count))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `(bit_position, flip_count)` pairs.
    #[must_use]
    pub fn bit_positions(&self) -> Vec<(u8, u32)> {
        self.0["bit_positions"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|entry| {
                        let arr = entry.as_array()?;
                        let pos = arr.first()?.as_u64()? as u8;
                        let count = arr.get(1)?.as_u64()? as u32;
                        Some((pos, count))
                    })
                    .collect()
            })
            .unwrap_or_default()
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
    pub fn new(unit_system: UnitSystem) -> Self {
        Self {
            unit_system,
            full: false,
        }
    }

    /// Full renderer including per-pattern results (after TUI exit).
    #[must_use]
    pub fn full(unit_system: UnitSystem) -> Self {
        Self {
            unit_system,
            full: true,
        }
    }

    /// Render the physical coverage block, including cross-run cumulative
    /// stats when a coverage store contributed them.
    fn render_coverage(&self, cov: &CoverageDoc<'_>, out: &mut dyn Write) -> io::Result<()> {
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
            let size = |b: u64| crate::units::Size::new(b as f64, self.unit_system);
            let mut breakdown = format!(
                "{} free + {} reclaimable + {} in-use + {} unreachable",
                size(gap.free_bytes()),
                size(gap.reclaimable_bytes()),
                size(gap.in_use_bytes()),
                size(gap.unreachable_bytes()),
            );
            if gap.unknown_bytes() > 0 {
                use std::fmt::Write as _;
                let _ = write!(breakdown, " + {} unknown", size(gap.unknown_bytes()));
            }
            writeln!(
                out,
                "  Untested:  {} = {breakdown}",
                size(gap.total_bytes())
            )?;
        }
        Ok(())
    }
}

impl ResultsRenderer for TableRenderer {
    fn render(&self, doc: &ResultsDoc, out: &mut dyn Write) -> io::Result<()> {
        let total_failures = doc.total_failures();
        let elapsed_ms = doc.elapsed_ms();
        let elapsed_secs = elapsed_ms / 1000.0;

        // Per-pass, per-pattern results (only in full mode)
        if self.full {
            let total_passes = doc.config().passes();
            for pass in doc.passes() {
                let mut pass_failures: u64 = 0;
                for p in pass.pattern_results() {
                    let failures = p.failure_count();
                    pass_failures += failures;
                    let elapsed_ms = p.elapsed_ms();
                    let throughput = crate::units::Rate::new(
                        p.bytes_processed() as f64 / (elapsed_ms / 1000.0),
                        self.unit_system,
                    );
                    // An interrupted pattern is incomplete: a clean result
                    // can't be trusted as a PASS, so flag it distinctly.
                    let interrupted = p.interrupted();
                    let suffix = if interrupted { "  (interrupted)" } else { "" };
                    if failures == 0 {
                        let label = if interrupted {
                            "INTR".yellow().bold().to_string()
                        } else {
                            "PASS".green().to_string()
                        };
                        writeln!(
                            out,
                            "  {} {:<20} {:>8.1}ms  {throughput:>}{suffix}",
                            label,
                            p.pattern_name(),
                            elapsed_ms,
                        )?;
                    } else {
                        writeln!(
                            out,
                            "  {} {:<20} {:>8.1}ms  {throughput:>}  ({failures} failures){suffix}",
                            "FAIL".red().bold(),
                            p.pattern_name(),
                            elapsed_ms,
                        )?;
                    }
                }
                let pass_number = pass.pass_number();
                if pass_failures == 0 {
                    writeln!(
                        out,
                        "  Pass {pass_number}/{total_passes}: {}",
                        "all patterns passed".green(),
                    )?;
                } else {
                    writeln!(
                        out,
                        "  Pass {pass_number}/{total_passes}: {}",
                        format!("{pass_failures} total failure(s)").red().bold(),
                    )?;
                }
                writeln!(out)?;
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
                    .map(|(pos, count)| format!("bit {pos} ({count}x)"))
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
                    .map(|(name, count)| format!("{name}: {count}"))
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
        let elapsed_display = if elapsed_secs < 1.0 {
            format!("{elapsed_ms:.0}ms")
        } else {
            format!("{elapsed_secs:.1}s")
        };

        if total_failures == 0 {
            writeln!(
                out,
                "{} ({})",
                "All tests passed.".green().bold(),
                elapsed_display,
            )?;
        } else {
            writeln!(
                out,
                "{} ({})",
                format!("{total_failures} failure(s) detected.")
                    .red()
                    .bold(),
                elapsed_display,
            )?;
        }

        Ok(())
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

    use super::*;
    use crate::error_analysis;
    use crate::failure::FailureBuilder;
    use crate::pattern::Pattern;
    use crate::runner::{PassResult, PatternResult, RunConfig, RunResults};

    fn make_config() -> RunConfig {
        RunConfig {
            size: 8192,
            passes: 1,
            patterns: vec![Pattern::SolidBits],
            workers: 1,
        }
    }

    fn clean_results() -> RunResults {
        RunResults::from_passes(
            vec![PassResult {
                pass_number: 1,
                pattern_results: vec![PatternResult {
                    pattern: Pattern::SolidBits,
                    failures: vec![],
                    elapsed: Duration::from_millis(100),
                    bytes_processed: 8192,
                    interrupted: false,
                }],
                ecc_deltas: vec![],
            }],
            make_config(),
            Duration::from_millis(100),
        )
    }

    fn failing_results() -> RunResults {
        let failures = vec![
            FailureBuilder::default()
                .addr(0x1000)
                .expected(0x0)
                .actual(1 << 20)
                .phys(0x5000)
                .build(),
            FailureBuilder::default()
                .addr(0x2000)
                .expected(0x0)
                .actual(1 << 20)
                .phys(0x9000)
                .build(),
        ];
        let mut results = RunResults::from_passes(
            vec![PassResult {
                pass_number: 1,
                pattern_results: vec![PatternResult {
                    pattern: Pattern::SolidBits,
                    failures,
                    elapsed: Duration::from_millis(50),
                    bytes_processed: 8192,
                    interrupted: false,
                }],
                ecc_deltas: vec![],
            }],
            make_config(),
            Duration::from_millis(50),
        );
        error_analysis::analyze(&mut results);
        results
    }

    /// One pass, two patterns -- distinct timings and sizes so per-pattern
    /// rendering is observable.
    fn multi_pattern_results() -> RunResults {
        RunResults::from_passes(
            vec![PassResult {
                pass_number: 1,
                pattern_results: vec![
                    PatternResult {
                        pattern: Pattern::SolidBits,
                        failures: vec![],
                        elapsed: Duration::from_millis(100),
                        bytes_processed: 8192,
                        interrupted: false,
                    },
                    PatternResult {
                        pattern: Pattern::Checkerboard,
                        failures: vec![],
                        elapsed: Duration::from_millis(50),
                        bytes_processed: 4096,
                        interrupted: false,
                    },
                ],
                ecc_deltas: vec![],
            }],
            RunConfig {
                size: 16384,
                passes: 1,
                patterns: vec![Pattern::SolidBits, Pattern::Checkerboard],
                workers: 4,
            },
            Duration::from_millis(150),
        )
    }

    /// Clean results with measured coverage: 64 MiB tested of 32 GiB installed.
    fn covered_results() -> RunResults {
        let mut r = clean_results();
        r.coverage = crate::sysmem::Coverage::Measured {
            tested_bytes: 64 * 1024 * 1024,
            total_bytes: 32 * 1024 * 1024 * 1024,
            source: crate::sysmem::RamSource::ProcIomem,
            cumulative: None,
            gap: None,
        };
        r
    }

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

    mod results_doc {
        use assert2::{assert, check};

        use super::*;

        #[test]
        fn from_clean_results() {
            let doc = ResultsDoc::from_results(&clean_results());
            check!(doc.total_failures() == 0);
            assert!(doc.error_analysis().is_none());
            assert!(doc.elapsed_ms() > 0.0);
        }

        #[test]
        fn from_failing_results() {
            let doc = ResultsDoc::from_results(&failing_results());
            check!(doc.total_failures() == 2);
            let ea = doc.error_analysis().unwrap();
            assert!(ea.classification_str().contains("stuck bit"));
            check!(ea.union_xor_mask() == 1 << 20);
            check!(ea.lowest_phys() == Some(0x5000));
            check!(ea.highest_phys() == Some(0x9000));
        }

        #[test]
        fn passes_iteration() {
            let doc = ResultsDoc::from_results(&clean_results());
            let passes: Vec<_> = doc.passes().collect();
            check!(passes.len() == 1);
            check!(passes[0].pass_number() == 1);
            check!(passes[0].total_failures() == 0);
        }

        #[test]
        fn pattern_results_iteration() {
            let doc = ResultsDoc::from_results(&clean_results());
            let pass = doc.passes().next().unwrap();
            let patterns: Vec<_> = pass.pattern_results().collect();
            check!(patterns.len() == 1);
            check!(patterns[0].pattern_name() == "SolidBits");
            assert!(patterns[0].elapsed_ms() > 0.0);
            check!(patterns[0].bytes_processed() == 8192);
            check!(patterns[0].failure_count() == 0);
        }

        #[test]
        fn config_accessors() {
            let doc = ResultsDoc::from_results(&clean_results());
            let cfg = doc.config();
            check!(cfg.size() == 8192);
            check!(cfg.passes() == 1);
            check!(cfg.workers() == 1);
        }

        #[test]
        fn coverage_accessor_measured() {
            let doc = ResultsDoc::from_results(&covered_results());
            let cov = doc.coverage().unwrap();
            assert!(cov.is_measured());
            check!(cov.tested_bytes() == 64 * 1024 * 1024);
            check!(cov.total_bytes() == 32u64 * 1024 * 1024 * 1024);
            check!(cov.source_label() == "/proc/iomem");
            assert!((cov.percent() - 0.195_312_5).abs() < 1e-6);
        }

        #[test]
        fn coverage_accessor_unavailable() {
            let doc = ResultsDoc::from_results(&clean_results());
            let cov = doc.coverage().unwrap();
            assert!(!cov.is_measured());
        }

        #[test]
        fn coverage_absent_when_no_key() {
            let doc = ResultsDoc::from_json(serde_json::json!({}));
            assert!(doc.coverage().is_none());
        }

        #[test]
        fn from_json_roundtrip() {
            let results = clean_results();
            let value = serde_json::to_value(&results).unwrap();
            let doc = ResultsDoc::from_json(value);
            check!(doc.total_failures() == 0);
            check!(doc.config().size() == 8192);
        }

        #[test]
        fn from_file_roundtrip() {
            let results = failing_results();
            let path = std::env::temp_dir()
                .join(format!("ferrite_test_results_{}.json", std::process::id()));
            let json = serde_json::to_string_pretty(&results).unwrap();
            std::fs::write(&path, &json).unwrap();

            let doc = ResultsDoc::from_file(&path).unwrap();
            check!(doc.total_failures() == 2);
            assert!(doc.error_analysis().is_some());

            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn from_file_invalid_json() {
            let path =
                std::env::temp_dir().join(format!("ferrite_bad_json_{}.json", std::process::id()));
            std::fs::write(&path, "not json").unwrap();
            assert!(ResultsDoc::from_file(&path).is_err());
            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn from_file_missing() {
            assert!(
                ResultsDoc::from_file(Path::new("/tmp/nonexistent_ferrite_test.json")).is_err()
            );
        }

        #[test]
        fn error_analysis_per_pattern() {
            let doc = ResultsDoc::from_results(&failing_results());
            let ea = doc.error_analysis().unwrap();
            let pp = ea.per_pattern_failures();
            check!(pp.len() == 1);
            check!(pp[0].0 == "SolidBits");
            check!(pp[0].1 == 2);
        }

        #[test]
        fn error_analysis_bit_positions() {
            let doc = ResultsDoc::from_results(&failing_results());
            let ea = doc.error_analysis().unwrap();
            let bp = ea.bit_positions();
            check!(bp.len() == 1);
            check!(bp[0] == (20, 2));
        }

        #[test]
        fn missing_fields_return_defaults() {
            let doc = ResultsDoc::from_json(serde_json::json!({}));
            check!(doc.total_failures() == 0);
            assert!(doc.elapsed_ms().abs() < f64::EPSILON);
            assert!(doc.error_analysis().is_none());
            check!(doc.passes().count() == 0);
            check!(doc.config().size() == 0);
        }
    }

    mod classification_str {
        use assert2::{assert, check};
        use serde_json::json;

        use super::*;

        #[test]
        fn coupling() {
            let doc = ResultsDoc::from_json(json!({
                "error_analysis": {
                    "classification": "Coupling",
                    "union_xor_mask": 0,
                    "bit_positions": [],
                    "per_pattern_failures": []
                }
            }));
            let ea = doc.error_analysis().unwrap();
            check!(ea.classification_str() == "coupling/disturbance errors");
        }

        #[test]
        fn mixed() {
            let doc = ResultsDoc::from_json(json!({
                "error_analysis": {
                    "classification": "Mixed",
                    "union_xor_mask": 0,
                    "bit_positions": [],
                    "per_pattern_failures": []
                }
            }));
            let ea = doc.error_analysis().unwrap();
            check!(ea.classification_str() == "mixed (stuck + coupling)");
        }

        #[test]
        fn stuck_bit() {
            let doc = ResultsDoc::from_json(json!({
                "error_analysis": {
                    "classification": {"StuckBit": {"positions": [5, 20]}},
                    "union_xor_mask": 0,
                    "bit_positions": [],
                    "per_pattern_failures": []
                }
            }));
            let ea = doc.error_analysis().unwrap();
            let s = ea.classification_str();
            assert!(s.contains("stuck bit"));
            assert!(s.contains("bit 5"));
            assert!(s.contains("bit 20"));
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
            r.coverage.attach_cumulative(crate::sysmem::Cumulative {
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
            r.coverage.attach_gap(crate::gap::GapReport {
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
            r.coverage.attach_gap(crate::gap::GapReport {
                unknown_bytes: 4096,
                ..crate::gap::GapReport::default()
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
            // 100ms < 1s so should show as "100ms"
            assert!(out.contains("ms"));
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
            assert!(out.contains("5.0s"));
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
