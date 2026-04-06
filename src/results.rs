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
    pub fn regions(&self) -> u64 {
        self.0["regions"].as_u64().unwrap_or(1)
    }

    #[must_use]
    pub fn parallel(&self) -> bool {
        self.0["parallel"].as_bool().unwrap_or(true)
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
pub struct TableRenderer {
    #[allow(dead_code)]
    unit_system: UnitSystem,
}

impl TableRenderer {
    #[must_use]
    pub fn new(unit_system: UnitSystem) -> Self {
        Self { unit_system }
    }
}

impl ResultsRenderer for TableRenderer {
    fn render(&self, doc: &ResultsDoc, out: &mut dyn Write) -> io::Result<()> {
        let total_failures = doc.total_failures();
        let elapsed_ms = doc.elapsed_ms();
        let elapsed_secs = elapsed_ms / 1000.0;

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
            regions: 1,
            parallel: false,
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
                }],
                ecc_deltas: vec![],
            }],
            make_config(),
            Duration::from_millis(50),
        );
        error_analysis::analyze(&mut results);
        results
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
            check!(cfg.regions() == 1);
            check!(!cfg.parallel());
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

        #[test]
        fn clean_run_shows_passed() {
            let out = render_to_string(&clean_results());
            assert!(out.contains("All tests passed"));
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
    }
}
