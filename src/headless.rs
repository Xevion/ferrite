use std::fmt;
use std::io::Write;
use std::time::Duration;

use owo_colors::OwoColorize;

use crate::Failure;
use crate::dimm::DimmTopology;
use crate::edac::EccDelta;
use crate::events::{EventRx, RegionEvent, RunEvent};
use crate::pattern::Pattern;
use crate::phys::MapStats;
use crate::units::{Rate, Size, UnitSystem};

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

/// Headless live event consumer that prints human-readable output from the
/// event bus during a test run.
///
/// Replaces `OutputSink::print_*()` methods. Writes directly to a `Write`
/// target (typically stdout). Active when `--format table` (the default).
pub struct HeadlessPrinter<W: Write> {
    out: W,
    unit_system: UnitSystem,
    total_passes: usize,
}

impl<W: Write> HeadlessPrinter<W> {
    /// Create a new headless printer writing to the given output.
    pub fn new(out: W, unit_system: UnitSystem) -> Self {
        Self {
            out,
            unit_system,
            total_passes: 0,
        }
    }

    /// Consume events from the receiver until `RunComplete` or channel disconnect.
    pub fn consume(&mut self, rx: &EventRx) {
        while let Ok(event) = rx.recv() {
            self.handle_event(&event);
            if matches!(event, RunEvent::RunComplete) {
                break;
            }
        }
    }

    /// Handle a single event. Exposed for testing and composability.
    pub fn handle_event(&mut self, event: &RunEvent) {
        match event {
            RunEvent::RunStart {
                size,
                passes,
                patterns,
                parallel,
                ..
            } => {
                self.total_passes = *passes;
                self.print_banner(*size, *passes, patterns.len(), *parallel);
            }
            RunEvent::MapInfo { stats } => {
                self.print_map_info(stats);
            }
            RunEvent::DimmInfo { topology } => {
                self.print_dimm_info(topology);
            }
            RunEvent::Region(
                _,
                RegionEvent::TestComplete {
                    pattern,
                    elapsed,
                    bytes,
                    failures,
                    ..
                },
            ) => {
                self.print_test_result(*pattern, *elapsed, *bytes, failures);
            }
            RunEvent::Region(_, RegionEvent::EccDeltas { pass, deltas }) => {
                self.print_ecc_deltas(*pass, deltas);
            }
            RunEvent::Region(_, RegionEvent::PassComplete { pass, failures, .. }) => {
                self.print_pass_summary(*pass, self.total_passes, *failures);
            }
            RunEvent::RunComplete
            | RunEvent::Log { .. }
            | RunEvent::Region(
                _,
                RegionEvent::PassStart { .. }
                | RegionEvent::TestStart { .. }
                | RegionEvent::Progress { .. },
            ) => {}
        }
    }

    /// Print the final result line after all events have been consumed.
    pub fn print_final_result(&mut self, total_failures: usize) {
        if total_failures == 0 {
            let _ = writeln!(self.out, "{}", "All tests passed.".green().bold());
        } else {
            let _ = writeln!(
                self.out,
                "{}",
                format!("{total_failures} failure(s) detected.")
                    .red()
                    .bold(),
            );
        }
    }

    fn print_banner(&mut self, size: usize, passes: usize, pattern_count: usize, parallel: bool) {
        let size_display = Size::new(size as f64, self.unit_system);
        let suffix = if parallel { "" } else { "  (sequential)" };
        let _ = writeln!(
            self.out,
            "{} Testing {size_display:.1} across {} pass(es) with {} pattern(s){}",
            "ferrite".bold(),
            passes,
            pattern_count,
            suffix,
        );
    }

    fn print_map_info(&mut self, stats: &MapStats) {
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
        let _ = writeln!(self.out, "  Physical address map: {}", parts.join(", "));
    }

    fn print_dimm_info(&mut self, topology: &DimmTopology) {
        let _ = writeln!(self.out, "  Installed DIMMs:");
        for entry in &topology.dimms {
            let _ = writeln!(self.out, "    {entry}");
        }
    }

    fn print_test_result(
        &mut self,
        pattern: Pattern,
        elapsed: Duration,
        bytes_processed: u64,
        failures: &[Failure],
    ) {
        let ms = elapsed.as_secs_f64() * 1000.0;
        let throughput = Rate::new(
            bytes_processed as f64 / elapsed.as_secs_f64(),
            self.unit_system,
        );
        if failures.is_empty() {
            let _ = writeln!(
                self.out,
                "  {} {:<20} {:>8.1}ms  {throughput:>}",
                "PASS".green(),
                pattern.to_string(),
                ms,
            );
        } else {
            let _ = writeln!(
                self.out,
                "  {} {:<20} {:>8.1}ms  {throughput:>}  ({} failures)",
                "FAIL".red().bold(),
                pattern.to_string(),
                ms,
                failures.len(),
            );
            let _ = write!(self.out, "{}", Truncated(failures, 5));
        }
    }

    fn print_ecc_deltas(&mut self, pass: usize, deltas: &[EccDelta]) {
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
            let _ = writeln!(
                self.out,
                "  {} ECC pass {}: {} on {}",
                "ECC".yellow().bold(),
                pass,
                parts.join(", "),
                label,
            );
        }
    }

    fn print_pass_summary(&mut self, pass: usize, total_passes: usize, failures: usize) {
        if failures == 0 {
            let _ = writeln!(
                self.out,
                "  Pass {}/{}: {}",
                pass,
                total_passes,
                "all patterns passed".green(),
            );
        } else {
            let _ = writeln!(
                self.out,
                "  Pass {}/{}: {}",
                pass,
                total_passes,
                format!("{failures} total failure(s)").red().bold(),
            );
        }
        let _ = writeln!(self.out);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use assert2::{assert, check};

    use super::*;
    use crate::edac::EccDelta;
    use crate::failure::FailureBuilder;
    use crate::phys::MapStats;

    fn printer() -> HeadlessPrinter<Vec<u8>> {
        HeadlessPrinter::new(Vec::new(), UnitSystem::Binary)
    }

    fn output(p: &HeadlessPrinter<Vec<u8>>) -> String {
        String::from_utf8_lossy(&p.out).to_string()
    }

    #[test]
    fn banner_parallel() {
        let mut p = printer();
        p.handle_event(&RunEvent::RunStart {
            size: 1024 * 1024 * 1024,
            passes: 2,
            patterns: vec![Pattern::SolidBits, Pattern::Checkerboard],
            regions: 1,
            parallel: true,
        });
        let out = output(&p);
        assert!(out.contains("ferrite"));
        assert!(out.contains("2 pass(es)"));
        assert!(out.contains("2 pattern(s)"));
        assert!(!out.contains("sequential"));
    }

    #[test]
    fn banner_sequential() {
        let mut p = printer();
        p.handle_event(&RunEvent::RunStart {
            size: 1024 * 1024,
            passes: 1,
            patterns: vec![Pattern::SolidBits],
            regions: 1,
            parallel: false,
        });
        let out = output(&p);
        assert!(out.contains("(sequential)"));
    }

    #[test]
    fn map_info_all_page_types() {
        let mut p = printer();
        p.handle_event(&RunEvent::MapInfo {
            stats: MapStats {
                total_pages: 100,
                huge_pages: 5,
                thp_pages: 10,
                hwpoison_pages: 1,
                unevictable_pages: 90,
            },
        });
        let out = output(&p);
        assert!(out.contains("100 pages mapped"));
        assert!(out.contains("10 THP"));
        assert!(out.contains("5 huge"));
        assert!(out.contains("hw-poisoned"));
    }

    #[test]
    fn map_info_minimal() {
        let mut p = printer();
        p.handle_event(&RunEvent::MapInfo {
            stats: MapStats {
                total_pages: 50,
                huge_pages: 0,
                thp_pages: 0,
                hwpoison_pages: 0,
                unevictable_pages: 50,
            },
        });
        let out = output(&p);
        assert!(out.contains("50 pages mapped"));
        assert!(!out.contains("THP"));
        assert!(!out.contains("huge"));
    }

    #[test]
    fn test_result_pass() {
        let mut p = printer();
        p.handle_event(&RunEvent::Region(
            0,
            RegionEvent::TestComplete {
                pattern: Pattern::SolidBits,
                pass: 1,
                elapsed: Duration::from_millis(100),
                bytes: 1024 * 1024,
                failures: vec![],
            },
        ));
        let out = output(&p);
        assert!(out.contains("PASS"));
        assert!(out.contains("Solid Bits"));
    }

    #[test]
    fn test_result_fail() {
        let mut p = printer();
        let failures = vec![
            FailureBuilder::default()
                .addr(0x1000)
                .expected(0xFF)
                .actual(0xFE)
                .build(),
        ];
        p.handle_event(&RunEvent::Region(
            0,
            RegionEvent::TestComplete {
                pattern: Pattern::WalkingOnes,
                pass: 1,
                elapsed: Duration::from_millis(50),
                bytes: 512 * 1024,
                failures,
            },
        ));
        let out = output(&p);
        assert!(out.contains("FAIL"));
        assert!(out.contains("1 failures"));
    }

    #[test]
    fn ecc_deltas() {
        let mut p = printer();
        p.handle_event(&RunEvent::Region(
            0,
            RegionEvent::EccDeltas {
                pass: 1,
                deltas: vec![
                    EccDelta {
                        mc: 0,
                        dimm_index: 0,
                        label: Some("DIMM_A1".to_owned()),
                        ce_delta: 3,
                        ue_delta: 0,
                    },
                    EccDelta {
                        mc: 0,
                        dimm_index: 1,
                        label: None,
                        ce_delta: 0,
                        ue_delta: 1,
                    },
                ],
            },
        ));
        let out = output(&p);
        assert!(out.contains("ECC"));
        assert!(out.contains("3 correctable"));
        assert!(out.contains("DIMM_A1"));
        assert!(out.contains("uncorrectable"));
        assert!(out.contains("mc0/dimm1"));
    }

    #[test]
    fn pass_summary_clean() {
        let mut p = printer();
        p.total_passes = 2;
        p.handle_event(&RunEvent::Region(
            0,
            RegionEvent::PassComplete {
                pass: 1,
                failures: 0,
                elapsed: Duration::from_secs(5),
            },
        ));
        let out = output(&p);
        assert!(out.contains("Pass 1/2"));
        assert!(out.contains("all patterns passed"));
    }

    #[test]
    fn pass_summary_failures() {
        let mut p = printer();
        p.total_passes = 3;
        p.handle_event(&RunEvent::Region(
            0,
            RegionEvent::PassComplete {
                pass: 2,
                failures: 5,
                elapsed: Duration::from_secs(10),
            },
        ));
        let out = output(&p);
        assert!(out.contains("Pass 2/3"));
        assert!(out.contains("5 total failure(s)"));
    }

    #[test]
    fn final_result_pass() {
        let mut p = printer();
        p.print_final_result(0);
        let out = output(&p);
        assert!(out.contains("All tests passed"));
    }

    #[test]
    fn final_result_fail() {
        let mut p = printer();
        p.print_final_result(7);
        let out = output(&p);
        assert!(out.contains("7 failure(s) detected"));
    }

    #[test]
    fn consume_full_sequence() {
        let (tx, rx) = crate::events::event_bus();
        tx.send(RunEvent::RunStart {
            size: 1024,
            passes: 1,
            patterns: vec![Pattern::SolidBits],
            regions: 1,
            parallel: true,
        })
        .unwrap();
        tx.send(RunEvent::Region(
            0,
            RegionEvent::PassStart {
                pass: 1,
                total_passes: 1,
            },
        ))
        .unwrap();
        tx.send(RunEvent::Region(
            0,
            RegionEvent::TestComplete {
                pattern: Pattern::SolidBits,
                pass: 1,
                elapsed: Duration::from_millis(10),
                bytes: 2048,
                failures: vec![],
            },
        ))
        .unwrap();
        tx.send(RunEvent::Region(
            0,
            RegionEvent::PassComplete {
                pass: 1,
                failures: 0,
                elapsed: Duration::from_millis(10),
            },
        ))
        .unwrap();
        tx.send(RunEvent::RunComplete).unwrap();

        let mut p = printer();
        p.consume(&rx);
        let out = output(&p);
        assert!(out.contains("ferrite"));
        assert!(out.contains("PASS"));
        assert!(out.contains("all patterns passed"));
    }

    #[test]
    fn ignored_events_produce_no_output() {
        let mut p = printer();
        p.handle_event(&RunEvent::Region(
            0,
            RegionEvent::PassStart {
                pass: 1,
                total_passes: 1,
            },
        ));
        p.handle_event(&RunEvent::Region(
            0,
            RegionEvent::TestStart {
                pattern: Pattern::SolidBits,
                pass: 1,
            },
        ));
        p.handle_event(&RunEvent::Region(
            0,
            RegionEvent::Progress {
                pattern: Pattern::SolidBits,
                pass: 1,
                sub_pass: 1,
                total: 2,
            },
        ));
        p.handle_event(&RunEvent::Log {
            level: tracing::Level::INFO,
            target: "test".to_owned(),
            message: "msg".to_owned(),
            fields: serde_json::json!({}),
        });
        check!(p.out.is_empty());
    }

    #[test]
    fn truncated_display_within_limit() {
        let failures: Vec<Failure> = (0..3)
            .map(|i| {
                FailureBuilder::default()
                    .addr(i * 8)
                    .expected(0xFF)
                    .actual(0xFE)
                    .build()
            })
            .collect();
        let t = Truncated(&failures, 5);
        let s = format!("{t}");
        assert!(!s.contains("more"));
        check!(s.lines().count() == 3);
    }

    #[test]
    fn truncated_display_exceeds_limit() {
        let failures: Vec<Failure> = (0..10)
            .map(|i| {
                FailureBuilder::default()
                    .addr(i * 8)
                    .expected(0xFF)
                    .actual(0xFE)
                    .build()
            })
            .collect();
        let t = Truncated(&failures, 3);
        let s = format!("{t}");
        assert!(s.contains("...+7 more"));
    }

    #[test]
    fn truncated_display_empty() {
        let failures: Vec<Failure> = vec![];
        let t = Truncated(&failures, 5);
        let s = format!("{t}");
        assert!(s.is_empty());
    }

    #[test]
    fn dimm_info_printed() {
        use crate::dimm::{DimmEntry, DimmTopology};
        use crate::edac::DimmEdac;

        let mut p = printer();
        let topology = DimmTopology {
            dimms: vec![DimmEntry {
                edac: Some(DimmEdac {
                    mc: 0,
                    dimm_index: 0,
                    label: Some("DIMM_A1".to_owned()),
                    location: None,
                    ce_count: 0,
                    ue_count: 0,
                }),
                smbios: None,
            }],
        };
        p.handle_event(&RunEvent::DimmInfo { topology });
        let out = output(&p);
        assert!(out.contains("Installed DIMMs"));
        assert!(out.contains("DIMM_A1"));
    }

    #[test]
    fn consume_stops_on_disconnect() {
        let (tx, rx) = crate::events::event_bus();
        tx.send(RunEvent::RunStart {
            size: 1024,
            passes: 1,
            patterns: vec![],
            regions: 1,
            parallel: true,
        })
        .unwrap();
        drop(tx);

        let mut p = printer();
        p.consume(&rx);
        let out = output(&p);
        assert!(out.contains("ferrite"));
    }
}
