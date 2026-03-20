use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::time::Duration;

use indicatif::{MultiProgress, ProgressDrawTarget};
use owo_colors::OwoColorize;
use serde::Serialize;

use crate::Failure;
use crate::pattern::Pattern;
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
    RunSummary {
        passes: usize,
        total_failures: usize,
        duration_ms: f64,
    },
}

#[derive(Serialize)]
struct FailureRecord {
    addr: String,
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
            expected: format!("0x{:016x}", f.expected),
            actual: format!("0x{:016x}", f.actual),
            xor: format!("0x{:016x}", f.xor()),
            flipped_bits: f.flipped_bits(),
            word_index: f.word_index,
        }
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
        writer: BufWriter<Box<dyn Write>>,
        mp: MultiProgress,
        unit_system: UnitSystem,
        /// True when JSON is written to stdout (human output goes to stderr).
        /// False when JSON is written to a file (human output goes to stdout).
        json_to_stdout: bool,
    },
}

impl OutputSink {
    /// Create a human-readable output sink.
    pub fn human(unit_system: UnitSystem) -> Self {
        Self::Human {
            mp: MultiProgress::new(),
            unit_system,
        }
    }

    /// Create a JSON output sink.
    ///
    /// - `"-"` or empty -> NDJSON to stdout, progress bars to stderr
    /// - any other path -> NDJSON to file, progress bars to stdout
    pub fn json(path: &str, unit_system: UnitSystem) -> io::Result<Self> {
        let to_stdout = path.is_empty() || path == "-";
        let writer: Box<dyn Write> = if to_stdout {
            Box::new(io::stdout().lock())
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
        })
    }

    /// Get a reference to the `MultiProgress` for creating progress bars.
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

    /// Print a test result line (PASS/FAIL) in human-readable format.
    /// Uses the progress bar's println to avoid clobbering the bar on stdout.
    /// When JSON owns stdout, prints to stderr instead.
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
                for f in failures {
                    eprintln!("       {f}");
                }
            } else {
                pb.println(line);
                for f in failures {
                    pb.println(format!("       {f}"));
                }
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
        if let Self::Json { writer, .. } = self {
            let _ = serde_json::to_writer(&mut *writer, event);
            let _ = writer.write_all(b"\n");
            let _ = writer.flush();
        }
    }
}
