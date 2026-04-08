use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::time::Duration;

use serde::Serialize;

use crate::Failure;
use crate::edac::EccDelta;
use crate::events::{RegionEvent, RunEvent};
use crate::phys::MapStats;

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

/// NDJSON event writer that serializes [`RunEvent`]s as newline-delimited JSON.
///
/// Each event is written as a single JSON object followed by a newline. The
/// writer handles broken-pipe errors gracefully, suppressing further writes
/// after the first pipe closure.
pub struct NdjsonEventWriter {
    writer: BufWriter<Box<dyn Write + Send>>,
    broken_pipe: bool,
}

impl NdjsonEventWriter {
    /// Create a writer from a boxed `Write` target.
    #[must_use]
    pub fn new(writer: Box<dyn Write + Send>) -> Self {
        Self {
            writer: BufWriter::new(writer),
            broken_pipe: false,
        }
    }

    /// Create a writer from a path string.
    ///
    /// - `"-"` or `""` writes to stdout
    /// - Any other path creates/truncates a file
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] if the output file cannot be created.
    pub fn from_path(path: &str) -> io::Result<Self> {
        let writer: Box<dyn Write + Send> = if path.is_empty() || path == "-" {
            Box::new(io::stdout())
        } else {
            Box::new(File::create(path)?)
        };
        Ok(Self::new(writer))
    }

    /// Process a single [`RunEvent`], writing the corresponding NDJSON event.
    ///
    /// Events that don't map to the NDJSON schema are silently ignored.
    pub fn handle_event(&mut self, event: &RunEvent) {
        match event {
            RunEvent::MapInfo { stats } => self.write_map_info(stats),
            RunEvent::Region(_, RegionEvent::PassStart { pass, total_passes }) => {
                self.write_event(&Event::PassStart {
                    pass: *pass,
                    total_passes: *total_passes,
                });
            }
            RunEvent::Region(_, RegionEvent::TestStart { pattern, pass }) => {
                self.write_event(&Event::TestStart {
                    pattern: pattern.to_string(),
                    pass: *pass,
                });
            }
            RunEvent::Region(
                _,
                RegionEvent::Progress {
                    pattern,
                    pass,
                    sub_pass,
                    total,
                },
            ) => {
                self.write_event(&Event::Progress {
                    pattern: pattern.to_string(),
                    pass: *pass,
                    sub_pass: *sub_pass,
                    total_sub_passes: *total,
                });
            }
            RunEvent::Region(
                _,
                RegionEvent::TestComplete {
                    pattern,
                    pass,
                    elapsed,
                    bytes,
                    failures,
                },
            ) => {
                let duration_ms = elapsed.as_secs_f64() * 1000.0;
                if failures.is_empty() {
                    self.write_event(&Event::TestPass {
                        pattern: pattern.to_string(),
                        pass: *pass,
                        duration_ms,
                        bytes_processed: *bytes,
                    });
                } else {
                    self.write_event(&Event::TestFail {
                        pattern: pattern.to_string(),
                        pass: *pass,
                        duration_ms,
                        bytes_processed: *bytes,
                        failures: failures.iter().map(FailureRecord::from).collect(),
                    });
                }
            }
            RunEvent::Region(
                _,
                RegionEvent::PassComplete {
                    pass,
                    failures,
                    elapsed,
                },
            ) => {
                self.write_event(&Event::PassComplete {
                    pass: *pass,
                    failures: *failures,
                    duration_ms: elapsed.as_secs_f64() * 1000.0,
                });
            }
            RunEvent::Region(_, RegionEvent::EccDeltas { pass, deltas }) => {
                self.write_event(&Event::EccDeltas {
                    pass: *pass,
                    deltas: deltas.clone(),
                });
            }
            _ => {}
        }
    }

    /// Write the run summary event (emitted after all events are consumed).
    pub fn write_summary(&mut self, passes: usize, total_failures: usize, elapsed: Duration) {
        self.write_event(&Event::RunSummary {
            passes,
            total_failures,
            duration_ms: elapsed.as_secs_f64() * 1000.0,
        });
    }

    fn write_map_info(&mut self, stats: &MapStats) {
        self.write_event(&Event::MapInfo {
            total_pages: stats.total_pages,
            huge_pages: stats.huge_pages,
            thp_pages: stats.thp_pages,
            hwpoison_pages: stats.hwpoison_pages,
            unevictable_pages: stats.unevictable_pages,
        });
    }

    fn write_event(&mut self, event: &Event) {
        if self.broken_pipe {
            return;
        }

        if let Err(e) = serde_json::to_writer(&mut self.writer, event) {
            if e.io_error_kind() == Some(std::io::ErrorKind::BrokenPipe) {
                eprintln!("warning: JSON output pipe closed; no further events will be written");
                self.broken_pipe = true;
            } else {
                eprintln!("warning: failed to write JSON event: {e}");
            }
            return;
        }

        if let Err(e) = self.writer.write_all(b"\n") {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                eprintln!("warning: JSON output pipe closed; no further events will be written");
                self.broken_pipe = true;
            } else {
                eprintln!("warning: failed to write JSON newline: {e}");
            }
            return;
        }

        if let Err(e) = self.writer.flush() {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                eprintln!("warning: JSON output pipe closed; no further events will be written");
                self.broken_pipe = true;
            } else {
                eprintln!("warning: failed to flush JSON output: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use assert2::{assert, check};

    use super::*;
    use crate::events::RegionEvent;
    use crate::pattern::Pattern;
    use crate::phys::{MapStats, PhysAddr};

    fn test_writer() -> (NdjsonEventWriter, std::path::PathBuf) {
        let path =
            std::env::temp_dir().join(format!("ferrite_test_ndjson_{}.ndjson", std::process::id()));
        let writer = NdjsonEventWriter::from_path(path.to_str().unwrap()).unwrap();
        (writer, path)
    }

    fn read_events(path: &std::path::Path) -> Vec<serde_json::Value> {
        let content = std::fs::read_to_string(path).unwrap();
        content
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn pass_start_event() {
        let (mut w, path) = test_writer();
        w.handle_event(&RunEvent::Region(
            0,
            RegionEvent::PassStart {
                pass: 1,
                total_passes: 3,
            },
        ));
        drop(w);

        let events = read_events(&path);
        check!(events.len() == 1);
        check!(events[0]["event"] == "pass_start");
        check!(events[0]["pass"] == 1);
        check!(events[0]["total_passes"] == 3);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_start_event() {
        let (mut w, path) = test_writer();
        w.handle_event(&RunEvent::Region(
            0,
            RegionEvent::TestStart {
                pattern: Pattern::SolidBits,
                pass: 1,
            },
        ));
        drop(w);

        let events = read_events(&path);
        check!(events[0]["event"] == "test_start");
        check!(events[0]["pattern"] == "Solid Bits");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_complete_pass() {
        let (mut w, path) = test_writer();
        w.handle_event(&RunEvent::Region(
            0,
            RegionEvent::TestComplete {
                pattern: Pattern::Checkerboard,
                pass: 1,
                elapsed: Duration::from_millis(100),
                bytes: 1024,
                failures: vec![],
            },
        ));
        drop(w);

        let events = read_events(&path);
        check!(events[0]["event"] == "test_pass");
        check!(events[0]["pattern"] == "Checkerboard");
        assert!(events[0]["duration_ms"].as_f64().unwrap() > 0.0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_complete_fail() {
        let (mut w, path) = test_writer();
        let failures = vec![Failure {
            addr: 0x1000,
            expected: 0xFF,
            actual: 0xFE,
            word_index: 0,
            phys_addr: Some(PhysAddr(0xABCD)),
        }];
        w.handle_event(&RunEvent::Region(
            0,
            RegionEvent::TestComplete {
                pattern: Pattern::WalkingOnes,
                pass: 2,
                elapsed: Duration::from_millis(50),
                bytes: 512,
                failures,
            },
        ));
        drop(w);

        let events = read_events(&path);
        check!(events[0]["event"] == "test_fail");
        let f = &events[0]["failures"][0];
        check!(f["flipped_bits"] == 1);
        assert!(f["phys_addr"].as_str().is_some());
        check!(f["word_index"] == 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn pass_complete_event() {
        let (mut w, path) = test_writer();
        w.handle_event(&RunEvent::Region(
            0,
            RegionEvent::PassComplete {
                pass: 1,
                failures: 5,
                elapsed: Duration::from_secs(2),
            },
        ));
        drop(w);

        let events = read_events(&path);
        check!(events[0]["event"] == "pass_complete");
        check!(events[0]["failures"] == 5);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn summary_event() {
        let (mut w, path) = test_writer();
        w.write_summary(3, 0, Duration::from_secs(10));
        drop(w);

        let events = read_events(&path);
        check!(events[0]["event"] == "run_summary");
        check!(events[0]["passes"] == 3);
        check!(events[0]["total_failures"] == 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn map_info_event() {
        let (mut w, path) = test_writer();
        w.handle_event(&RunEvent::MapInfo {
            stats: MapStats {
                total_pages: 100,
                huge_pages: 5,
                thp_pages: 10,
                hwpoison_pages: 0,
                unevictable_pages: 90,
            },
        });
        drop(w);

        let events = read_events(&path);
        check!(events[0]["event"] == "map_info");
        check!(events[0]["total_pages"] == 100);
        check!(events[0]["huge_pages"] == 5);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ecc_deltas_event() {
        let (mut w, path) = test_writer();
        w.handle_event(&RunEvent::Region(
            0,
            RegionEvent::EccDeltas {
                pass: 1,
                deltas: vec![crate::edac::EccDelta {
                    mc: 0,
                    dimm_index: 1,
                    label: Some("DIMM_A1".to_owned()),
                    ce_delta: 2,
                    ue_delta: 0,
                }],
            },
        ));
        drop(w);

        let events = read_events(&path);
        check!(events[0]["event"] == "ecc_deltas");
        check!(events[0]["deltas"][0]["ce_delta"] == 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn progress_event() {
        let (mut w, path) = test_writer();
        w.handle_event(&RunEvent::Region(
            0,
            RegionEvent::Progress {
                pattern: Pattern::StuckAddress,
                pass: 1,
                sub_pass: 3,
                total: 5,
            },
        ));
        drop(w);

        let events = read_events(&path);
        check!(events[0]["event"] == "progress");
        check!(events[0]["sub_pass"] == 3);
        check!(events[0]["total_sub_passes"] == 5);
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
        check!(r.word_index == 5);
        check!(r.flipped_bits == f.flipped_bits());
    }

    #[test]
    fn ignored_events_produce_no_output() {
        let (mut w, path) = test_writer();
        w.handle_event(&RunEvent::RunStart {
            size: 1024,
            passes: 1,
            patterns: vec![],
            regions: 1,
            parallel: true,
        });
        w.handle_event(&RunEvent::RunComplete);
        drop(w);

        let events = read_events(&path);
        check!(events.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn multiple_events_sequence() {
        let (mut w, path) = test_writer();
        w.handle_event(&RunEvent::Region(
            0,
            RegionEvent::PassStart {
                pass: 1,
                total_passes: 2,
            },
        ));
        w.handle_event(&RunEvent::Region(
            0,
            RegionEvent::TestStart {
                pattern: Pattern::SolidBits,
                pass: 1,
            },
        ));
        w.handle_event(&RunEvent::Region(
            0,
            RegionEvent::TestComplete {
                pattern: Pattern::SolidBits,
                pass: 1,
                elapsed: Duration::from_millis(50),
                bytes: 1024,
                failures: vec![],
            },
        ));
        w.handle_event(&RunEvent::Region(
            0,
            RegionEvent::PassComplete {
                pass: 1,
                failures: 0,
                elapsed: Duration::from_millis(100),
            },
        ));
        w.write_summary(1, 0, Duration::from_millis(100));
        drop(w);

        let events = read_events(&path);
        check!(events.len() == 5);
        check!(events[0]["event"] == "pass_start");
        check!(events[1]["event"] == "test_start");
        check!(events[2]["event"] == "test_pass");
        check!(events[3]["event"] == "pass_complete");
        check!(events[4]["event"] == "run_summary");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn broken_pipe_suppresses_further_writes() {
        use std::io::{self, Write};

        struct BrokenWriter {
            call_count: std::cell::Cell<usize>,
        }

        impl Write for BrokenWriter {
            fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
                self.call_count.set(self.call_count.get() + 1);
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "pipe closed"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let writer: Box<dyn Write + Send> = Box::new(BrokenWriter {
            call_count: std::cell::Cell::new(0),
        });
        let mut w = NdjsonEventWriter {
            writer: std::io::BufWriter::with_capacity(0, writer),
            broken_pipe: false,
        };

        w.handle_event(&RunEvent::Region(
            0,
            RegionEvent::PassStart {
                pass: 1,
                total_passes: 1,
            },
        ));
        assert!(w.broken_pipe);

        // Second event should be suppressed
        w.handle_event(&RunEvent::Region(
            0,
            RegionEvent::PassStart {
                pass: 2,
                total_passes: 2,
            },
        ));
    }

    #[test]
    fn stdout_writer() {
        let w = NdjsonEventWriter::from_path("-").unwrap();
        assert!(!w.broken_pipe);
    }

    #[test]
    fn empty_path_is_stdout() {
        let w = NdjsonEventWriter::from_path("").unwrap();
        assert!(!w.broken_pipe);
    }
}
