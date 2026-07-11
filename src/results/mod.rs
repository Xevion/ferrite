use std::io;
use std::path::Path;

#[cfg(test)]
mod fixtures;
pub(crate) mod render;

pub use render::{JsonRenderer, ResultsRenderer, TableRenderer, render_ceiling_report};

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
    pub const fn from_json(value: serde_json::Value) -> Self {
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
    pub const fn as_value(&self) -> &serde_json::Value {
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

    /// True when `--max-errors` truncated this pattern's failure list.
    #[must_use]
    pub fn capped(&self) -> bool {
        self.0["capped"].as_bool().unwrap_or(false)
    }
}

/// Borrowed view into run configuration.
pub struct ConfigDoc<'a>(&'a serde_json::Value);

impl ConfigDoc<'_> {
    #[must_use]
    pub fn passes(&self) -> u64 {
        self.0["passes"].as_u64().unwrap_or(0)
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

#[cfg(test)]
mod tests {
    use super::fixtures::{clean_results, covered_results, failing_results};
    use super::*;

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
            check!(cfg.passes() == 1);
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
}
