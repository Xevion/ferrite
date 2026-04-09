use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::time::Duration;

use jiff::Timestamp;
use serde::Serialize;

use crate::Failure;
use crate::edac::EccDelta;
use crate::events::{RegionEvent, RunEvent};
use crate::phys::MapStats;

/// Current schema version for the NDJSON event stream.
const SCHEMA_VERSION: u32 = 1;

/// Stable NDJSON event types.
///
/// This is the **curated, versioned** projection of internal [`RunEvent`]s.
/// Changes to this enum should be intentional and versioned.
#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum Event {
    Header {
        schema_version: u32,
    },
    RunStart {
        size: usize,
        passes: usize,
        patterns: Vec<String>,
        regions: usize,
        parallel: bool,
    },
    MapInfo {
        total_pages: usize,
        huge_pages: usize,
        thp_pages: usize,
        hwpoison_pages: usize,
        unevictable_pages: usize,
    },
    DimmInfo {
        dimm_count: usize,
    },
    PassStart {
        region: usize,
        pass: usize,
        total_passes: usize,
    },
    TestStart {
        region: usize,
        pattern: String,
        pass: usize,
    },
    Progress {
        region: usize,
        pattern: String,
        pass: usize,
        sub_pass: u64,
        total_sub_passes: u64,
    },
    TestPass {
        region: usize,
        pattern: String,
        pass: usize,
        duration_ms: f64,
        bytes_processed: u64,
    },
    TestFail {
        region: usize,
        pattern: String,
        pass: usize,
        duration_ms: f64,
        bytes_processed: u64,
        failures: Vec<FailureRecord>,
    },
    PassComplete {
        region: usize,
        pass: usize,
        failures: usize,
        duration_ms: f64,
    },
    EccDeltas {
        region: usize,
        pass: usize,
        deltas: Vec<EccDelta>,
    },
    Log {
        level: String,
        target: String,
        message: String,
        #[serde(skip_serializing_if = "is_empty_object")]
        fields: serde_json::Value,
    },
    RunComplete {
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

fn is_empty_object(v: &serde_json::Value) -> bool {
    matches!(v, serde_json::Value::Object(m) if m.is_empty())
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
/// Each event is written as a single JSON object with an ISO 8601 timestamp,
/// followed by a newline. The first line is always a `header` event containing
/// the schema version. The writer handles broken-pipe errors gracefully,
/// suppressing further writes after the first pipe closure.
pub struct NdjsonEventWriter {
    writer: BufWriter<Box<dyn Write + Send>>,
    broken_pipe: bool,
}

impl NdjsonEventWriter {
    /// Create a writer from a boxed `Write` target.
    ///
    /// Immediately writes a `header` event with the current schema version.
    #[must_use]
    pub fn new(writer: Box<dyn Write + Send>) -> Self {
        let mut this = Self {
            writer: BufWriter::new(writer),
            broken_pipe: false,
        };
        this.write_event(&Event::Header {
            schema_version: SCHEMA_VERSION,
        });
        this
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
    /// Events that don't map to the stable NDJSON schema are silently ignored.
    pub fn handle_event(&mut self, event: &RunEvent) {
        match event {
            RunEvent::RunStart {
                size,
                passes,
                patterns,
                regions,
                parallel,
            } => {
                self.write_event(&Event::RunStart {
                    size: *size,
                    passes: *passes,
                    patterns: patterns.iter().map(ToString::to_string).collect(),
                    regions: *regions,
                    parallel: *parallel,
                });
            }
            RunEvent::MapInfo { stats } => self.write_map_info(stats),
            RunEvent::DimmInfo { topology } => {
                self.write_event(&Event::DimmInfo {
                    dimm_count: topology.dimms.len(),
                });
            }
            RunEvent::Region(idx, region_event) => {
                self.handle_region_event(*idx, region_event);
            }
            RunEvent::Log {
                level,
                target,
                message,
                fields,
            } => {
                self.write_event(&Event::Log {
                    level: level.to_string(),
                    target: target.clone(),
                    message: message.clone(),
                    fields: fields.clone(),
                });
            }
            RunEvent::RunComplete => {}
        }
    }

    /// Write the run-complete summary event (emitted after all events are consumed).
    pub fn write_run_complete(&mut self, passes: usize, total_failures: usize, elapsed: Duration) {
        self.write_event(&Event::RunComplete {
            passes,
            total_failures,
            duration_ms: elapsed.as_secs_f64() * 1000.0,
        });
    }

    fn handle_region_event(&mut self, region: usize, event: &RegionEvent) {
        match event {
            RegionEvent::PassStart { pass, total_passes } => {
                self.write_event(&Event::PassStart {
                    region,
                    pass: *pass,
                    total_passes: *total_passes,
                });
            }
            RegionEvent::TestStart { pattern, pass } => {
                self.write_event(&Event::TestStart {
                    region,
                    pattern: pattern.to_string(),
                    pass: *pass,
                });
            }
            RegionEvent::Progress {
                pattern,
                pass,
                sub_pass,
                total,
            } => {
                self.write_event(&Event::Progress {
                    region,
                    pattern: pattern.to_string(),
                    pass: *pass,
                    sub_pass: *sub_pass,
                    total_sub_passes: *total,
                });
            }
            RegionEvent::TestComplete {
                pattern,
                pass,
                elapsed,
                bytes,
                failures,
            } => {
                let duration_ms = elapsed.as_secs_f64() * 1000.0;
                if failures.is_empty() {
                    self.write_event(&Event::TestPass {
                        region,
                        pattern: pattern.to_string(),
                        pass: *pass,
                        duration_ms,
                        bytes_processed: *bytes,
                    });
                } else {
                    self.write_event(&Event::TestFail {
                        region,
                        pattern: pattern.to_string(),
                        pass: *pass,
                        duration_ms,
                        bytes_processed: *bytes,
                        failures: failures.iter().map(FailureRecord::from).collect(),
                    });
                }
            }
            RegionEvent::PassComplete {
                pass,
                failures,
                elapsed,
            } => {
                self.write_event(&Event::PassComplete {
                    region,
                    pass: *pass,
                    failures: *failures,
                    duration_ms: elapsed.as_secs_f64() * 1000.0,
                });
            }
            RegionEvent::EccDeltas { pass, deltas } => {
                self.write_event(&Event::EccDeltas {
                    region,
                    pass: *pass,
                    deltas: deltas.clone(),
                });
            }
        }
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

        let mut value = serde_json::to_value(event).expect("Event serialization is infallible");
        value["timestamp"] = serde_json::Value::String(Timestamp::now().to_string());

        if let Err(e) = serde_json::to_writer(&mut self.writer, &value) {
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use assert2::{assert, check};

    use super::*;
    use crate::events::RegionEvent;
    use crate::pattern::Pattern;
    use crate::phys::{MapStats, PhysAddr};

    static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn test_writer() -> (NdjsonEventWriter, std::path::PathBuf) {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "ferrite_test_ndjson_{}_{n}.ndjson",
            std::process::id()
        ));
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

    /// Helper: validate that an event has a valid ISO 8601 timestamp.
    fn assert_has_timestamp(event: &serde_json::Value) {
        let ts = event["timestamp"]
            .as_str()
            .expect("missing timestamp field");
        // jiff Timestamp format: "2026-04-08T21:53:42.123456789Z"
        assert!(ts.ends_with('Z'), "timestamp should be UTC: {ts}");
        assert!(ts.contains('T'), "timestamp should be ISO 8601: {ts}");
    }

    #[test]
    fn header_emitted_on_construction() {
        let (w, path) = test_writer();
        drop(w);

        let events = read_events(&path);
        check!(events.len() == 1);
        check!(events[0]["event"] == "header");
        check!(events[0]["schema_version"] == 1);
        assert_has_timestamp(&events[0]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn run_start_event() {
        let (mut w, path) = test_writer();
        w.handle_event(&RunEvent::RunStart {
            size: 1_073_741_824,
            passes: 2,
            patterns: vec![Pattern::SolidBits, Pattern::WalkingOnes],
            regions: 4,
            parallel: true,
        });
        drop(w);

        let events = read_events(&path);
        // header + run_start
        check!(events.len() == 2);
        let e = &events[1];
        check!(e["event"] == "run_start");
        check!(e["size"] == 1_073_741_824);
        check!(e["passes"] == 2);
        check!(e["regions"] == 4);
        check!(e["parallel"] == true);
        check!(e["patterns"].as_array().unwrap().len() == 2);
        assert_has_timestamp(e);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn run_complete_event() {
        let (mut w, path) = test_writer();
        w.write_run_complete(3, 5, Duration::from_secs(10));
        drop(w);

        let events = read_events(&path);
        // header + run_complete
        check!(events.len() == 2);
        let e = &events[1];
        check!(e["event"] == "run_complete");
        check!(e["passes"] == 3);
        check!(e["total_failures"] == 5);
        assert!(e["duration_ms"].as_f64().unwrap() > 0.0);
        assert_has_timestamp(e);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn run_complete_internal_event_ignored() {
        let (mut w, path) = test_writer();
        w.handle_event(&RunEvent::RunComplete);
        drop(w);

        let events = read_events(&path);
        // Only header — RunComplete from event bus is a no-op;
        // the summary is written explicitly via write_run_complete.
        check!(events.len() == 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn dimm_info_event() {
        use crate::dimm::{DimmEntry, DimmTopology};

        let (mut w, path) = test_writer();
        let topology = DimmTopology {
            dimms: vec![
                DimmEntry {
                    edac: None,
                    smbios: None,
                },
                DimmEntry {
                    edac: None,
                    smbios: None,
                },
            ],
        };
        w.handle_event(&RunEvent::DimmInfo { topology });
        drop(w);

        let events = read_events(&path);
        check!(events.len() == 2);
        let e = &events[1];
        check!(e["event"] == "dimm_info");
        check!(e["dimm_count"] == 2);
        assert_has_timestamp(e);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn log_event() {
        let (mut w, path) = test_writer();
        w.handle_event(&RunEvent::Log {
            level: tracing::Level::WARN,
            target: "ferrite::runner".to_owned(),
            message: "something happened".to_owned(),
            fields: serde_json::json!({"key": "value"}),
        });
        drop(w);

        let events = read_events(&path);
        check!(events.len() == 2);
        let e = &events[1];
        check!(e["event"] == "log");
        check!(e["level"] == "WARN");
        check!(e["target"] == "ferrite::runner");
        check!(e["message"] == "something happened");
        check!(e["fields"]["key"] == "value");
        assert_has_timestamp(e);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn log_event_empty_fields_omitted() {
        let (mut w, path) = test_writer();
        w.handle_event(&RunEvent::Log {
            level: tracing::Level::INFO,
            target: "ferrite".to_owned(),
            message: "no fields".to_owned(),
            fields: serde_json::json!({}),
        });
        drop(w);

        let events = read_events(&path);
        let e = &events[1];
        check!(e["event"] == "log");
        check!(!e.as_object().unwrap().contains_key("fields"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn pass_start_includes_region() {
        let (mut w, path) = test_writer();
        w.handle_event(&RunEvent::Region(
            3,
            RegionEvent::PassStart {
                pass: 1,
                total_passes: 3,
            },
        ));
        drop(w);

        let events = read_events(&path);
        check!(events.len() == 2);
        let e = &events[1];
        check!(e["event"] == "pass_start");
        check!(e["region"] == 3);
        check!(e["pass"] == 1);
        check!(e["total_passes"] == 3);
        assert_has_timestamp(e);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_start_includes_region() {
        let (mut w, path) = test_writer();
        w.handle_event(&RunEvent::Region(
            2,
            RegionEvent::TestStart {
                pattern: Pattern::SolidBits,
                pass: 1,
            },
        ));
        drop(w);

        let events = read_events(&path);
        let e = &events[1];
        check!(e["event"] == "test_start");
        check!(e["region"] == 2);
        check!(e["pattern"] == "Solid Bits");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_pass_includes_region() {
        let (mut w, path) = test_writer();
        w.handle_event(&RunEvent::Region(
            1,
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
        let e = &events[1];
        check!(e["event"] == "test_pass");
        check!(e["region"] == 1);
        check!(e["pattern"] == "Checkerboard");
        assert!(e["duration_ms"].as_f64().unwrap() > 0.0);
        assert_has_timestamp(e);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_fail_includes_region_and_failures() {
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
        let e = &events[1];
        check!(e["event"] == "test_fail");
        check!(e["region"] == 0);
        let f = &e["failures"][0];
        check!(f["flipped_bits"] == 1);
        assert!(f["phys_addr"].as_str().is_some());
        check!(f["word_index"] == 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn pass_complete_includes_region() {
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
        let e = &events[1];
        check!(e["event"] == "pass_complete");
        check!(e["region"] == 0);
        check!(e["failures"] == 5);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn progress_includes_region() {
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
        let e = &events[1];
        check!(e["event"] == "progress");
        check!(e["region"] == 0);
        check!(e["sub_pass"] == 3);
        check!(e["total_sub_passes"] == 5);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ecc_deltas_includes_region() {
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
        let e = &events[1];
        check!(e["event"] == "ecc_deltas");
        check!(e["region"] == 0);
        check!(e["deltas"][0]["ce_delta"] == 2);
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
        check!(events.len() == 2);
        let e = &events[1];
        check!(e["event"] == "map_info");
        check!(e["total_pages"] == 100);
        check!(e["huge_pages"] == 5);
        assert_has_timestamp(e);
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
    fn full_event_sequence() {
        let (mut w, path) = test_writer();
        w.handle_event(&RunEvent::RunStart {
            size: 1024,
            passes: 1,
            patterns: vec![Pattern::SolidBits],
            regions: 1,
            parallel: false,
        });
        w.handle_event(&RunEvent::Region(
            0,
            RegionEvent::PassStart {
                pass: 1,
                total_passes: 1,
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
        w.write_run_complete(1, 0, Duration::from_millis(100));
        drop(w);

        let events = read_events(&path);
        check!(events.len() == 7);
        check!(events[0]["event"] == "header");
        check!(events[1]["event"] == "run_start");
        check!(events[2]["event"] == "pass_start");
        check!(events[3]["event"] == "test_start");
        check!(events[4]["event"] == "test_pass");
        check!(events[5]["event"] == "pass_complete");
        check!(events[6]["event"] == "run_complete");

        // Every event has a timestamp
        for e in &events {
            assert_has_timestamp(e);
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn broken_pipe_suppresses_further_writes() {
        use std::io::{self, Write};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        struct BrokenWriter {
            call_count: Arc<AtomicUsize>,
        }

        impl Write for BrokenWriter {
            fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
                self.call_count.fetch_add(1, AtomicOrdering::Relaxed);
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "pipe closed"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let count = Arc::new(AtomicUsize::new(0));
        let writer: Box<dyn Write + Send> = Box::new(BrokenWriter {
            call_count: Arc::clone(&count),
        });
        // Construct directly to bypass header write (which would break on BrokenWriter)
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
        let count_after_break = count.load(AtomicOrdering::Relaxed);
        check!(count_after_break > 0);

        // Second event should be suppressed — no additional write calls
        w.handle_event(&RunEvent::Region(
            0,
            RegionEvent::PassStart {
                pass: 2,
                total_passes: 2,
            },
        ));
        check!(count.load(AtomicOrdering::Relaxed) == count_after_break);
    }

    #[test]
    fn stdout_writer() {
        let w = NdjsonEventWriter::from_path("-").unwrap();
        check!(!w.broken_pipe);
    }

    #[test]
    fn empty_path_is_stdout() {
        let w = NdjsonEventWriter::from_path("").unwrap();
        check!(!w.broken_pipe);
    }
}
