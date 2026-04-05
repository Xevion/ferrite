use std::fmt;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::time::Duration;

use indicatif::{MultiProgress, ProgressDrawTarget};
use owo_colors::OwoColorize;
use serde::Serialize;

use crate::Failure;
use crate::edac::EccDelta;
use crate::pattern::Pattern;
use crate::phys::MapStats;
use crate::units::{Rate, Size, UnitSystem};

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum Event {
    PassStart {
        pass: usize,
        total_passes: usize,
    },
    TestStart {
        pattern: String,
        pass: usize,
    },
    Progress {
        pattern: String,
        pass: usize,
        sub_pass: u64,
        total_sub_passes: u64,
    },
    TestPass {
        pattern: String,
        pass: usize,
        duration_ms: f64,
        bytes_processed: u64,
    },
    TestFail {
        pattern: String,
        pass: usize,
        duration_ms: f64,
        bytes_processed: u64,
        failures: Vec<FailureRecord>,
    },
    PassComplete {
        pass: usize,
        failures: usize,
        duration_ms: f64,
    },
    EccDeltas {
        pass: usize,
        deltas: Vec<EccDelta>,
    },
    MapInfo {
        total_pages: usize,
        huge_pages: usize,
        thp_pages: usize,
        hwpoison_pages: usize,
        unevictable_pages: usize,
    },
    RunSummary {
        passes: usize,
        total_failures: usize,
        duration_ms: f64,
    },
}

#[derive(Serialize)]
struct FailureRecord {
    addr: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    phys_addr: Option<String>,
    expected: String,
    actual: String,
    xor: String,
    flipped_bits: u32,
    word_index: usize,
}

impl From<&Failure> for FailureRecord {
    fn from(f: &Failure) -> Self {
        Self {
            addr: format!("0x{:016x}", f.addr),
            phys_addr: f.phys_addr.map(|p| format!("{p}")),
            expected: format!("0x{:016x}", f.expected),
            actual: format!("0x{:016x}", f.actual),
            xor: format!("0x{:016x}", f.xor()),
            flipped_bits: f.flipped_bits(),
            word_index: f.word_index,
        }
    }
}

/// Display wrapper that shows the first `N` failures then a count of the remainder.
struct Truncated<'a>(&'a [Failure], usize);

impl fmt::Display for Truncated<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let shown = self.0.len().min(self.1);
        for failure in &self.0[..shown] {
            writeln!(f, "       {failure}")?;
        }
        let remaining = self.0.len() - shown;
        if remaining > 0 {
            write!(f, "       ...+{remaining} more")?;
        }
        Ok(())
    }
}

/// Controls where output goes -- human-readable to terminal or NDJSON events.
pub enum OutputSink {
    /// Standard human-readable output. Progress bars and text go to stdout.
    Human {
        mp: MultiProgress,
        unit_system: UnitSystem,
    },
    /// NDJSON events to a writer. Progress bars go to the terminal via the
    /// contained `MultiProgress` (which targets stderr when JSON goes to
    /// stdout, or stdout when JSON goes to a file).
    Json {
        writer: BufWriter<Box<dyn Write + Send>>,
        mp: MultiProgress,
        unit_system: UnitSystem,
        /// True when JSON is written to stdout (human output goes to stderr).
        /// False when JSON is written to a file (human output goes to stdout).
        json_to_stdout: bool,
        /// Set after the first broken-pipe error to suppress per-event warnings.
        broken_pipe: bool,
    },
}

impl OutputSink {
    /// Create a human-readable output sink.
    #[must_use]
    pub fn human(unit_system: UnitSystem) -> Self {
        Self::Human {
            mp: MultiProgress::new(),
            unit_system,
        }
    }

    /// Create a JSON output sink.
    ///
    /// - `"-"` or `""` -> NDJSON to stdout, human output to stderr
    /// - any other path -> NDJSON to file, human output to stdout
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] if the output file cannot be created.
    pub fn json(path: &str, unit_system: UnitSystem) -> io::Result<Self> {
        let to_stdout = path.is_empty() || path == "-";
        let writer: Box<dyn Write + Send> = if to_stdout {
            Box::new(io::stdout())
        } else {
            Box::new(File::create(path)?)
        };
        let mp = if to_stdout {
            MultiProgress::with_draw_target(ProgressDrawTarget::stderr())
        } else {
            MultiProgress::new()
        };
        Ok(Self::Json {
            writer: BufWriter::new(writer),
            mp,
            unit_system,
            json_to_stdout: to_stdout,
            broken_pipe: false,
        })
    }

    /// Get a reference to the `MultiProgress` for creating progress bars.
    #[must_use]
    pub fn multi_progress(&self) -> &MultiProgress {
        match self {
            Self::Human { mp, .. } | Self::Json { mp, .. } => mp,
        }
    }

    fn unit_system(&self) -> UnitSystem {
        match self {
            Self::Human { unit_system, .. } | Self::Json { unit_system, .. } => *unit_system,
        }
    }

    /// Whether this sink emits JSON events.
    #[must_use]
    pub fn is_json(&self) -> bool {
        matches!(self, Self::Json { .. })
    }

    /// Whether human-readable output should go to stderr.
    /// True when JSON is writing to stdout; false otherwise.
    fn human_to_stderr(&self) -> bool {
        matches!(
            self,
            Self::Json {
                json_to_stdout: true,
                ..
            }
        )
    }

    pub fn emit_pass_start(&mut self, pass: usize, total_passes: usize) {
        self.write_event(&Event::PassStart { pass, total_passes });
    }

    pub fn emit_test_start(&mut self, pattern: Pattern, pass: usize) {
        self.write_event(&Event::TestStart {
            pattern: pattern.to_string(),
            pass,
        });
    }

    pub fn emit_progress(
        &mut self,
        pattern: Pattern,
        pass: usize,
        sub_pass: u64,
        total_sub_passes: u64,
    ) {
        self.write_event(&Event::Progress {
            pattern: pattern.to_string(),
            pass,
            sub_pass,
            total_sub_passes,
        });
    }

    pub fn emit_test_complete(
        &mut self,
        pattern: Pattern,
        pass: usize,
        elapsed: Duration,
        bytes_processed: u64,
        failures: &[Failure],
    ) {
        let duration_ms = elapsed.as_secs_f64() * 1000.0;
        if failures.is_empty() {
            self.write_event(&Event::TestPass {
                pattern: pattern.to_string(),
                pass,
                duration_ms,
                bytes_processed,
            });
        } else {
            self.write_event(&Event::TestFail {
                pattern: pattern.to_string(),
                pass,
                duration_ms,
                bytes_processed,
                failures: failures.iter().map(FailureRecord::from).collect(),
            });
        }
    }

    pub fn emit_pass_complete(&mut self, pass: usize, failures: usize, elapsed: Duration) {
        self.write_event(&Event::PassComplete {
            pass,
            failures,
            duration_ms: elapsed.as_secs_f64() * 1000.0,
        });
    }

    pub fn emit_ecc_deltas(&mut self, pass: usize, deltas: &[EccDelta]) {
        self.write_event(&Event::EccDeltas {
            pass,
            deltas: deltas.to_vec(),
        });
    }

    pub fn emit_map_info(&mut self, stats: &MapStats) {
        self.write_event(&Event::MapInfo {
            total_pages: stats.total_pages,
            huge_pages: stats.huge_pages,
            thp_pages: stats.thp_pages,
            hwpoison_pages: stats.hwpoison_pages,
            unevictable_pages: stats.unevictable_pages,
        });
    }

    pub fn emit_summary(&mut self, passes: usize, total_failures: usize, elapsed: Duration) {
        self.write_event(&Event::RunSummary {
            passes,
            total_failures,
            duration_ms: elapsed.as_secs_f64() * 1000.0,
        });
    }

    /// Print the initial banner line.
    pub fn print_banner(
        &self,
        region_bytes: usize,
        passes: usize,
        pattern_count: usize,
        parallel: bool,
    ) {
        let size = Size::new(region_bytes as f64, self.unit_system());
        let suffix = if parallel { "" } else { "  (sequential)" };
        let line = format!(
            "{} Testing {size:.1} across {} pass(es) with {} pattern(s){}\n",
            "ferrite".bold(),
            passes,
            pattern_count,
            suffix,
        );
        if self.human_to_stderr() {
            eprint!("{line}");
        } else {
            print!("{line}");
        }
    }

    /// Print page map stats after building the physical address map.
    pub fn print_map_info(&self, stats: &MapStats) {
        let mut parts = vec![format!("{} pages mapped", stats.total_pages)];
        if stats.thp_pages > 0 {
            parts.push(format!("{} THP", stats.thp_pages));
        }
        if stats.huge_pages > 0 {
            parts.push(format!("{} huge", stats.huge_pages));
        }
        if stats.hwpoison_pages > 0 {
            parts.push(format!("{} {}", stats.hwpoison_pages, "hw-poisoned".red()));
        }
        let line = format!("  Physical address map: {}", parts.join(", "));
        if self.human_to_stderr() {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    }

    /// Print a test result line (PASS/FAIL) in human-readable format.
    pub fn print_test_result(
        &self,
        pattern: Pattern,
        elapsed: Duration,
        bytes_processed: u64,
        failures: &[Failure],
        pb: &indicatif::ProgressBar,
    ) {
        let ms = elapsed.as_secs_f64() * 1000.0;
        let throughput = Rate::new(
            bytes_processed as f64 / elapsed.as_secs_f64(),
            self.unit_system(),
        );
        if failures.is_empty() {
            let line = format!(
                "  {} {:<20} {:>8.1}ms  {throughput:>}",
                "PASS".green(),
                pattern.to_string(),
                ms,
            );
            if self.human_to_stderr() {
                eprintln!("{line}");
            } else {
                pb.println(line);
            }
        } else {
            let line = format!(
                "  {} {:<20} {:>8.1}ms  {throughput:>}  ({} errors)",
                "FAIL".red().bold(),
                pattern.to_string(),
                ms,
                failures.len(),
            );
            if self.human_to_stderr() {
                eprintln!("{line}");
                eprint!("{}", Truncated(failures, 5));
            } else {
                pb.println(line);
                pb.println(format!("{}", Truncated(failures, 5)));
            }
        }
    }

    /// Print ECC delta summary for a pass.
    pub fn print_ecc_deltas(&self, pass: usize, deltas: &[EccDelta]) {
        for d in deltas {
            let fallback = format!("mc{}/dimm{}", d.mc, d.dimm_index);
            let label = d.label.as_deref().unwrap_or(&fallback);
            let mut parts = Vec::new();
            if d.ce_delta > 0 {
                parts.push(format!("{} correctable", d.ce_delta));
            }
            if d.ue_delta > 0 {
                parts.push(format!("{} {}", d.ue_delta, "uncorrectable".red().bold()));
            }
            let line = format!(
                "  {} ECC pass {}: {} on {}",
                "ECC".yellow().bold(),
                pass,
                parts.join(", "),
                label,
            );
            if self.human_to_stderr() {
                eprintln!("{line}");
            } else {
                println!("{line}");
            }
        }
    }

    /// Print the per-pass summary line.
    pub fn print_pass_summary(&self, pass: usize, total_passes: usize, failures: usize) {
        let line = if failures == 0 {
            format!(
                "  Pass {}/{}: {}",
                pass,
                total_passes,
                "all patterns passed".green(),
            )
        } else {
            format!(
                "  Pass {}/{}: {}",
                pass,
                total_passes,
                format!("{failures} total failure(s)").red().bold(),
            )
        };
        if self.human_to_stderr() {
            eprintln!("{line}");
            eprintln!();
        } else {
            println!("{line}");
            println!();
        }
    }

    /// Print the final result line.
    pub fn print_final_result(&self, total_failures: usize) {
        let line = if total_failures == 0 {
            format!("{}", "All tests passed.".green().bold())
        } else {
            format!(
                "{}",
                format!("{total_failures} failure(s) detected.")
                    .red()
                    .bold(),
            )
        };
        if self.human_to_stderr() {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    }

    fn write_event(&mut self, event: &Event) {
        let Self::Json {
            writer,
            broken_pipe,
            ..
        } = self
        else {
            return;
        };

        if *broken_pipe {
            return;
        }

        if let Err(e) = serde_json::to_writer(&mut *writer, event) {
            if e.io_error_kind() == Some(std::io::ErrorKind::BrokenPipe) {
                eprintln!("warning: JSON output pipe closed; no further events will be written");
                *broken_pipe = true;
            } else {
                eprintln!("warning: failed to write JSON event: {e}");
            }
            return;
        }

        if let Err(e) = writer.write_all(b"\n") {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                eprintln!("warning: JSON output pipe closed; no further events will be written");
                *broken_pipe = true;
            } else {
                eprintln!("warning: failed to write JSON newline: {e}");
            }
            return;
        }

        if let Err(e) = writer.flush() {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                eprintln!("warning: JSON output pipe closed; no further events will be written");
                *broken_pipe = true;
            } else {
                eprintln!("warning: failed to flush JSON output: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phys::PhysAddr;
    use std::time::Duration;

    /// Create a JSON sink that writes to a temp file, returning (sink, path).
    fn json_sink() -> (OutputSink, std::path::PathBuf) {
        let path =
            std::env::temp_dir().join(format!("ferrite_test_output_{}.ndjson", std::process::id()));
        let sink = OutputSink::json(path.to_str().unwrap(), UnitSystem::Binary).unwrap();
        (sink, path)
    }

    /// Read the temp file and parse each line as JSON.
    fn read_events(path: &std::path::Path) -> Vec<serde_json::Value> {
        let content = std::fs::read_to_string(path).unwrap();
        content
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn human_sink_is_not_json() {
        let sink = OutputSink::human(UnitSystem::Binary);
        assert!(!sink.is_json());
    }

    #[test]
    fn json_sink_is_json() {
        let (sink, path) = json_sink();
        assert!(sink.is_json());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn emit_pass_start_writes_event() {
        let (mut sink, path) = json_sink();
        sink.emit_pass_start(1, 3);
        drop(sink);

        let events = read_events(&path);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event"], "pass_start");
        assert_eq!(events[0]["pass"], 1);
        assert_eq!(events[0]["total_passes"], 3);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn emit_test_start_writes_event() {
        let (mut sink, path) = json_sink();
        sink.emit_test_start(Pattern::SolidBits, 1);
        drop(sink);

        let events = read_events(&path);
        assert_eq!(events[0]["event"], "test_start");
        assert_eq!(events[0]["pattern"], "Solid Bits");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn emit_test_complete_pass() {
        let (mut sink, path) = json_sink();
        sink.emit_test_complete(
            Pattern::Checkerboard,
            1,
            Duration::from_millis(100),
            1024,
            &[],
        );
        drop(sink);

        let events = read_events(&path);
        assert_eq!(events[0]["event"], "test_pass");
        assert_eq!(events[0]["pattern"], "Checkerboard");
        assert!(events[0]["duration_ms"].as_f64().unwrap() > 0.0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn emit_test_complete_fail() {
        let (mut sink, path) = json_sink();
        let failures = vec![Failure {
            addr: 0x1000,
            expected: 0xFF,
            actual: 0xFE,
            word_index: 0,
            phys_addr: Some(PhysAddr(0xABCD)),
        }];
        sink.emit_test_complete(
            Pattern::WalkingOnes,
            2,
            Duration::from_millis(50),
            512,
            &failures,
        );
        drop(sink);

        let events = read_events(&path);
        assert_eq!(events[0]["event"], "test_fail");
        let f = &events[0]["failures"][0];
        assert_eq!(f["flipped_bits"], 1);
        assert!(f["phys_addr"].as_str().is_some());
        assert_eq!(f["word_index"], 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn emit_pass_complete_writes_event() {
        let (mut sink, path) = json_sink();
        sink.emit_pass_complete(1, 5, Duration::from_secs(2));
        drop(sink);

        let events = read_events(&path);
        assert_eq!(events[0]["event"], "pass_complete");
        assert_eq!(events[0]["failures"], 5);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn emit_summary_writes_event() {
        let (mut sink, path) = json_sink();
        sink.emit_summary(3, 0, Duration::from_secs(10));
        drop(sink);

        let events = read_events(&path);
        assert_eq!(events[0]["event"], "run_summary");
        assert_eq!(events[0]["passes"], 3);
        assert_eq!(events[0]["total_failures"], 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn emit_map_info_writes_event() {
        let (mut sink, path) = json_sink();
        let stats = MapStats {
            total_pages: 100,
            huge_pages: 5,
            thp_pages: 10,
            hwpoison_pages: 0,
            unevictable_pages: 90,
        };
        sink.emit_map_info(&stats);
        drop(sink);

        let events = read_events(&path);
        assert_eq!(events[0]["event"], "map_info");
        assert_eq!(events[0]["total_pages"], 100);
        assert_eq!(events[0]["huge_pages"], 5);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn emit_ecc_deltas_writes_event() {
        let (mut sink, path) = json_sink();
        let deltas = vec![crate::edac::EccDelta {
            mc: 0,
            dimm_index: 1,
            label: Some("DIMM_A1".to_owned()),
            ce_delta: 2,
            ue_delta: 0,
        }];
        sink.emit_ecc_deltas(1, &deltas);
        drop(sink);

        let events = read_events(&path);
        assert_eq!(events[0]["event"], "ecc_deltas");
        assert_eq!(events[0]["deltas"][0]["ce_delta"], 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn emit_progress_writes_event() {
        let (mut sink, path) = json_sink();
        sink.emit_progress(Pattern::StuckAddress, 1, 3, 5);
        drop(sink);

        let events = read_events(&path);
        assert_eq!(events[0]["event"], "progress");
        assert_eq!(events[0]["sub_pass"], 3);
        assert_eq!(events[0]["total_sub_passes"], 5);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn failure_record_from_without_phys() {
        let f = Failure {
            addr: 0x2000,
            expected: 0xAAAA,
            actual: 0xBBBB,
            word_index: 5,
            phys_addr: None,
        };
        let r = FailureRecord::from(&f);
        assert!(r.phys_addr.is_none());
        assert_eq!(r.word_index, 5);
        assert_eq!(r.flipped_bits, f.flipped_bits());
    }

    #[test]
    fn human_sink_emit_does_not_write_json() {
        // Human sink should silently ignore write_event calls (no crash, no output)
        let mut sink = OutputSink::human(UnitSystem::Decimal);
        sink.emit_pass_start(1, 1);
        sink.emit_summary(1, 0, Duration::from_secs(1));
        // No panic means success -- human sink just skips JSON writes
    }

    #[test]
    fn json_to_file_does_not_redirect_stderr() {
        let (sink, path) = json_sink();
        // When writing JSON to a file (not stdout), human_to_stderr should be false
        assert!(!sink.human_to_stderr());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn print_methods_do_not_panic() {
        let sink = OutputSink::human(UnitSystem::Binary);
        let stats = MapStats {
            total_pages: 50,
            huge_pages: 0,
            thp_pages: 10,
            hwpoison_pages: 1,
            unevictable_pages: 50,
        };
        // Exercise all print paths -- they write to stdout/stderr, which is fine
        sink.print_banner(1024 * 1024, 2, 5, true);
        sink.print_banner(1024 * 1024, 1, 3, false);
        sink.print_map_info(&stats);
        sink.print_pass_summary(1, 2, 0);
        sink.print_pass_summary(1, 2, 3);
        sink.print_final_result(0);
        sink.print_final_result(5);
    }

    #[test]
    fn print_ecc_deltas_exercises_all_branches() {
        let sink = OutputSink::human(UnitSystem::Binary);
        let deltas = vec![
            crate::edac::EccDelta {
                mc: 0,
                dimm_index: 0,
                label: Some("DIMM_A1".to_owned()),
                ce_delta: 3,
                ue_delta: 0,
            },
            crate::edac::EccDelta {
                mc: 0,
                dimm_index: 1,
                label: None,
                ce_delta: 0,
                ue_delta: 1,
            },
            crate::edac::EccDelta {
                mc: 1,
                dimm_index: 0,
                label: None,
                ce_delta: 2,
                ue_delta: 3,
            },
        ];
        sink.print_ecc_deltas(1, &deltas);
    }
}
