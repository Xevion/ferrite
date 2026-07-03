//! Physical memory accounting: the installed-RAM denominator and the
//! single-run coverage measurement built on top of it.
//!
//! The denominator comes from `/proc/iomem` "System RAM" ranges when readable
//! (requires root), falling back to `/proc/meminfo` `MemTotal` otherwise. The
//! numerator is the physical footprint actually resolved and tested this run,
//! supplied by [`crate::phys::MapStats`].

use serde::Serialize;

use crate::phys::MapStats;

/// Which source produced the installed-RAM denominator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RamSource {
    /// Summed `/proc/iomem` "System RAM" ranges (authoritative, needs root).
    ProcIomem,
    /// `/proc/meminfo` `MemTotal` -- a slight underestimate (excludes
    /// firmware/kernel-reserved regions), used when `/proc/iomem` is not
    /// readable as root.
    MemTotal,
}

/// Total installed (testable) physical RAM and the source it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstalledRam {
    pub bytes: u64,
    pub source: RamSource,
}

/// Single-run physical coverage: how much of installed RAM this run tested.
///
/// Serialized with a `status` tag so JSON consumers always see the field, even
/// when coverage could not be measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Coverage {
    /// Coverage was measured for this run.
    Measured {
        tested_bytes: u64,
        total_bytes: u64,
        source: RamSource,
    },
    /// Coverage could not be measured -- no physical address resolution
    /// (`--no-phys` or missing `CAP_SYS_ADMIN`), or no denominator available.
    Unavailable,
}

impl Coverage {
    /// Fraction of installed RAM tested, as a percentage (0.0..=100.0).
    /// `None` when unavailable or the denominator is zero.
    #[must_use]
    pub fn percent(&self) -> Option<f64> {
        match self {
            Self::Measured {
                tested_bytes,
                total_bytes,
                ..
            } if *total_bytes > 0 => Some(*tested_bytes as f64 / *total_bytes as f64 * 100.0),
            _ => None,
        }
    }
}

/// Assemble a [`Coverage`] from tested bytes and an optional denominator.
/// Returns [`Coverage::Unavailable`] when no denominator is available.
#[must_use]
pub fn measure(tested_bytes: u64, installed: Option<InstalledRam>) -> Coverage {
    match installed {
        Some(ram) => Coverage::Measured {
            tested_bytes,
            total_bytes: ram.bytes,
            source: ram.source,
        },
        None => Coverage::Unavailable,
    }
}

/// Measure coverage for a run given its page-map stats.
///
/// Reads the installed-RAM denominator from `/proc`. Returns
/// [`Coverage::Unavailable`] when physical resolution did not run.
#[cfg_attr(coverage_nightly, coverage(off))]
#[must_use]
pub fn coverage_for(map_stats: Option<&MapStats>) -> Coverage {
    match map_stats {
        Some(stats) => measure(stats.tested_bytes(), installed_ram()),
        None => Coverage::Unavailable,
    }
}

/// Read the installed-RAM denominator from `/proc/iomem` (preferred) and
/// `/proc/meminfo` (fallback). Returns `None` only if neither is available.
#[cfg_attr(coverage_nightly, coverage(off))]
#[must_use]
pub fn installed_ram() -> Option<InstalledRam> {
    let iomem_bytes =
        std::fs::read_to_string("/proc/iomem").map_or(0, |s| parse_iomem_system_ram(&s));
    let memtotal = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| parse_meminfo_memtotal(&s));
    select_installed_ram(iomem_bytes, memtotal)
}

/// Choose the denominator: `/proc/iomem` when it is readable and at least as
/// large as `MemTotal` (the root case), otherwise `MemTotal`.
///
/// Without root, every `/proc/iomem` address reads as zero, collapsing each
/// range to a single byte -- far below `MemTotal` -- so the comparison routes
/// to the fallback automatically.
#[must_use]
fn select_installed_ram(iomem_bytes: u64, memtotal: Option<u64>) -> Option<InstalledRam> {
    match memtotal {
        Some(mt) if iomem_bytes >= mt && iomem_bytes > 0 => Some(InstalledRam {
            bytes: iomem_bytes,
            source: RamSource::ProcIomem,
        }),
        Some(mt) => Some(InstalledRam {
            bytes: mt,
            source: RamSource::MemTotal,
        }),
        None if iomem_bytes > 0 => Some(InstalledRam {
            bytes: iomem_bytes,
            source: RamSource::ProcIomem,
        }),
        None => None,
    }
}

/// Sum the byte span of every top-level `/proc/iomem` "System RAM" range.
///
/// Ranges are inclusive (`start-end`), so a range spans `end - start + 1`
/// bytes. Nested child entries (e.g. "Kernel code") carry different labels and
/// are skipped, so there is no double counting.
#[must_use]
fn parse_iomem_system_ram(contents: &str) -> u64 {
    contents.lines().filter_map(parse_iomem_ram_line).sum()
}

/// Parse one `/proc/iomem` line, returning its byte span iff it is labeled
/// exactly "System RAM".
fn parse_iomem_ram_line(line: &str) -> Option<u64> {
    let (range, label) = line.split_once(':')?;
    if label.trim() != "System RAM" {
        return None;
    }
    let (start, end) = range.trim().split_once('-')?;
    let start = u64::from_str_radix(start.trim(), 16).ok()?;
    let end = u64::from_str_radix(end.trim(), 16).ok()?;
    end.checked_sub(start).map(|span| span + 1)
}

/// Parse `MemTotal` (in kB) from `/proc/meminfo`, returning bytes.
#[must_use]
fn parse_meminfo_memtotal(contents: &str) -> Option<u64> {
    parse_meminfo_kb(contents, "MemTotal")
}

/// Parse any `kB`-valued `/proc/meminfo` field, returning bytes.
#[must_use]
fn parse_meminfo_kb(contents: &str, field: &str) -> Option<u64> {
    contents.lines().find_map(|line| {
        let rest = line.strip_prefix(field)?.strip_prefix(':')?;
        let kb = rest.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()?;
        Some(kb * 1024)
    })
}

/// Read `MemAvailable` from `/proc/meminfo`, in bytes.
#[cfg_attr(coverage_nightly, coverage(off))]
#[must_use]
pub fn mem_available() -> Option<u64> {
    let contents = std::fs::read_to_string("/proc/meminfo").ok()?;
    parse_meminfo_kb(&contents, "MemAvailable")
}

/// Read `MemTotal` from `/proc/meminfo`, in bytes.
#[cfg_attr(coverage_nightly, coverage(off))]
#[must_use]
pub fn mem_total() -> Option<u64> {
    let contents = std::fs::read_to_string("/proc/meminfo").ok()?;
    parse_meminfo_kb(&contents, "MemTotal")
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    mod parse_iomem {
        use assert2::check;

        use super::*;

        #[test]
        fn sums_multiple_system_ram_ranges() {
            // 0x0000-0x0fff = 4096 bytes; 0x2000-0x2fff = 4096 bytes.
            let contents = "\
00000000-00000fff : System RAM
00001000-00001fff : Reserved
00002000-00002fff : System RAM
";
            check!(parse_iomem_system_ram(contents) == 8192);
        }

        #[test]
        fn ignores_non_system_ram_labels() {
            let contents = "\
000a0000-000bffff : PCI Bus 0000:00
000c0000-000c7fff : Video ROM
fed00000-fed003ff : HPET 0
";
            check!(parse_iomem_system_ram(contents) == 0);
        }

        #[test]
        fn ignores_indented_child_entries() {
            // Children under a System RAM range carry other labels and must not
            // be counted (they are subsets of their parent).
            let contents = "\
00100000-3fffffff : System RAM
  01000000-0159ffff : Kernel code
  01600000-019fffff : Kernel data
";
            check!(parse_iomem_system_ram(contents) == 0x3fff_ffff - 0x0010_0000 + 1);
        }

        #[test]
        fn non_root_zeroed_ranges_collapse_to_tiny_total() {
            // Without root, all addresses read as zero: each range spans 1 byte.
            let contents = "\
00000000-00000000 : System RAM
00000000-00000000 : System RAM
";
            check!(parse_iomem_system_ram(contents) == 2);
        }

        #[test]
        fn empty_input_is_zero() {
            check!(parse_iomem_system_ram("") == 0);
        }

        #[test]
        fn malformed_lines_are_skipped() {
            let contents = "\
garbage without a colon
zzzz-yyyy : System RAM
00000000-00000fff : System RAM
";
            check!(parse_iomem_system_ram(contents) == 4096);
        }
    }

    mod parse_meminfo {
        use assert2::check;

        use super::*;

        #[test]
        fn extracts_memtotal_as_bytes() {
            let contents = "\
MemTotal:       32797892 kB
MemFree:         1234567 kB
";
            check!(parse_meminfo_memtotal(contents) == Some(32_797_892 * 1024));
        }

        #[test]
        fn missing_memtotal_is_none() {
            let contents = "MemFree: 1234567 kB\n";
            check!(parse_meminfo_memtotal(contents) == None);
        }

        #[test]
        fn extracts_memavailable_as_bytes() {
            let contents = "\
MemTotal:       32781736 kB
MemFree:         1234567 kB
MemAvailable:   25512532 kB
";
            check!(parse_meminfo_kb(contents, "MemAvailable") == Some(25_512_532 * 1024));
        }

        #[test]
        fn field_name_must_match_exactly() {
            // "MemFree" must not match a query for "Mem".
            let contents = "MemFree: 1234567 kB\n";
            check!(parse_meminfo_kb(contents, "Mem") == None);
            check!(parse_meminfo_kb(contents, "MemAvailable") == None);
        }

        #[test]
        fn non_kb_field_is_none() {
            // HugePages_Total has no kB suffix; the parser only handles kB fields.
            let contents = "HugePages_Total:       0\n";
            check!(parse_meminfo_kb(contents, "HugePages_Total") == None);
        }

        #[test]
        fn empty_input_is_none() {
            check!(parse_meminfo_memtotal("") == None);
        }
    }

    mod gate {
        use assert2::check;

        use super::*;

        #[test]
        fn prefers_iomem_when_at_least_memtotal() {
            let ram = select_installed_ram(34_000_000_000, Some(32_000_000_000)).unwrap();
            check!(ram.source == RamSource::ProcIomem);
            check!(ram.bytes == 34_000_000_000);
        }

        #[test]
        fn falls_back_to_memtotal_when_iomem_smaller() {
            // Non-root: iomem collapses to a tiny total, below MemTotal.
            let ram = select_installed_ram(2, Some(32_000_000_000)).unwrap();
            check!(ram.source == RamSource::MemTotal);
            check!(ram.bytes == 32_000_000_000);
        }

        #[test]
        fn falls_back_to_memtotal_when_iomem_zero() {
            let ram = select_installed_ram(0, Some(16_000_000_000)).unwrap();
            check!(ram.source == RamSource::MemTotal);
        }

        #[test]
        fn uses_iomem_when_memtotal_missing() {
            let ram = select_installed_ram(8_000_000_000, None).unwrap();
            check!(ram.source == RamSource::ProcIomem);
            check!(ram.bytes == 8_000_000_000);
        }

        #[test]
        fn none_when_both_unavailable() {
            check!(select_installed_ram(0, None) == None);
        }
    }

    mod coverage {
        use assert2::check;

        use super::*;

        #[test]
        fn measure_with_denominator_is_measured() {
            let installed = InstalledRam {
                bytes: 32_000_000_000,
                source: RamSource::ProcIomem,
            };
            let cov = measure(64 * 1024 * 1024, Some(installed));
            check!(
                cov == Coverage::Measured {
                    tested_bytes: 64 * 1024 * 1024,
                    total_bytes: 32_000_000_000,
                    source: RamSource::ProcIomem,
                }
            );
        }

        #[test]
        fn measure_without_denominator_is_unavailable() {
            check!(measure(1024, None) == Coverage::Unavailable);
        }

        #[test]
        fn percent_is_ratio() {
            let cov = Coverage::Measured {
                tested_bytes: 1_000,
                total_bytes: 4_000,
                source: RamSource::ProcIomem,
            };
            // 1000 / 4000 * 100 = 25.0 exactly.
            check!(cov.percent() == Some(25.0));
        }

        #[test]
        fn percent_zero_denominator_is_none() {
            let cov = Coverage::Measured {
                tested_bytes: 100,
                total_bytes: 0,
                source: RamSource::MemTotal,
            };
            check!(cov.percent() == None);
        }

        #[test]
        fn percent_unavailable_is_none() {
            check!(Coverage::Unavailable.percent() == None);
        }

        #[test]
        fn coverage_for_none_map_stats_is_unavailable() {
            check!(coverage_for(None) == Coverage::Unavailable);
        }
    }

    mod serialization {
        use assert2::check;

        use super::*;

        #[test]
        fn measured_carries_status_and_source_tags() {
            let cov = Coverage::Measured {
                tested_bytes: 64,
                total_bytes: 128,
                source: RamSource::MemTotal,
            };
            let json = serde_json::to_value(cov).unwrap();
            check!(json["status"] == "measured");
            check!(json["tested_bytes"] == 64);
            check!(json["total_bytes"] == 128);
            check!(json["source"] == "mem_total");
        }

        #[test]
        fn unavailable_carries_status_tag() {
            let json = serde_json::to_value(Coverage::Unavailable).unwrap();
            check!(json["status"] == "unavailable");
        }
    }

    #[test]
    fn installed_ram_reads_something_on_linux() {
        // /proc/meminfo is world-readable on any Linux host, so the denominator
        // is always resolvable even in an unprivileged CI environment.
        let ram = installed_ram().expect("installed RAM should resolve on Linux");
        check!(ram.bytes > 0);
    }
}
