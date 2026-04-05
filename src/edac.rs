use std::fs;
use std::path::Path;
use std::time::Instant;

use serde::Serialize;

/// Per-DIMM EDAC information from sysfs.
#[derive(Debug, Clone, Serialize)]
pub struct DimmEdac {
    /// Memory controller index (e.g., 0 for mc0).
    pub mc: usize,
    /// DIMM index within the controller.
    pub dimm_index: usize,
    /// User-assigned or driver-populated label (often empty).
    pub label: Option<String>,
    /// Location string, e.g., "channel 0 slot 0".
    pub location: Option<String>,
    /// Correctable error count.
    pub ce_count: u64,
    /// Uncorrectable error count.
    pub ue_count: u64,
}

/// Snapshot of all EDAC error counters at a point in time.
#[derive(Debug, Clone)]
pub struct EdacSnapshot {
    pub dimms: Vec<DimmEdac>,
    pub timestamp: Instant,
}

/// Change in error counts between two snapshots.
#[derive(Debug, Clone, Serialize)]
pub struct EccDelta {
    pub mc: usize,
    pub dimm_index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub ce_delta: u64,
    pub ue_delta: u64,
}

const EDAC_MC_PATH: &str = "/sys/devices/system/edac/mc";

impl EdacSnapshot {
    /// Read current EDAC counters. Returns `None` if EDAC is not available.
    #[must_use]
    pub fn capture() -> Option<Self> {
        let mc_root = Path::new(EDAC_MC_PATH);
        if !mc_root.is_dir() {
            return None;
        }

        let mut dimms = Vec::new();

        for mc_entry in sorted_dir_entries(mc_root)? {
            let mc_name = mc_entry.file_name();
            let mc_name = mc_name.to_str()?;
            let mc_index = mc_name.strip_prefix("mc")?.parse::<usize>().ok()?;
            let mc_path = mc_entry.path();

            // Try modern dimm-based API first, then legacy csrow-based
            if !try_read_dimm_api(&mc_path, mc_index, &mut dimms) {
                try_read_csrow_api(&mc_path, mc_index, &mut dimms);
            }
        }

        if dimms.is_empty() {
            return None;
        }

        Some(Self {
            dimms,
            timestamp: Instant::now(),
        })
    }

    /// Compute deltas between this (before) and `after` snapshot.
    /// Only returns entries where at least one counter increased.
    #[must_use]
    pub fn delta(&self, after: &EdacSnapshot) -> Vec<EccDelta> {
        let mut deltas = Vec::new();
        for after_dimm in &after.dimms {
            if let Some(before_dimm) = self
                .dimms
                .iter()
                .find(|d| d.mc == after_dimm.mc && d.dimm_index == after_dimm.dimm_index)
            {
                let ce_delta = after_dimm.ce_count.saturating_sub(before_dimm.ce_count);
                let ue_delta = after_dimm.ue_count.saturating_sub(before_dimm.ue_count);
                if ce_delta > 0 || ue_delta > 0 {
                    deltas.push(EccDelta {
                        mc: after_dimm.mc,
                        dimm_index: after_dimm.dimm_index,
                        label: after_dimm.label.clone(),
                        ce_delta,
                        ue_delta,
                    });
                }
            }
        }
        deltas
    }
}

/// Modern EDAC API: mc0/dimm0/, mc0/dimm1/, etc.
fn try_read_dimm_api(mc_path: &Path, mc_index: usize, dimms: &mut Vec<DimmEdac>) -> bool {
    let mut found = false;
    let Some(entries) = sorted_dir_entries(mc_path) else {
        return false;
    };

    for entry in entries {
        let name = entry.file_name();
        let name = name.to_str().unwrap_or("");
        let Some(idx_str) = name.strip_prefix("dimm") else {
            continue;
        };
        let Ok(dimm_index) = idx_str.parse::<usize>() else {
            continue;
        };

        let dimm_path = entry.path();
        let ce = read_u64_file(&dimm_path.join("dimm_ce_count")).unwrap_or(0);
        let ue = read_u64_file(&dimm_path.join("dimm_ue_count")).unwrap_or(0);
        let label = read_trimmed(&dimm_path.join("dimm_label"));
        let location = read_trimmed(&dimm_path.join("dimm_location"));

        dimms.push(DimmEdac {
            mc: mc_index,
            dimm_index,
            label,
            location,
            ce_count: ce,
            ue_count: ue,
        });
        found = true;
    }
    found
}

/// Legacy EDAC API: `mc0/csrow0/ch0_ce_count`, `mc0/csrow0/ch0_dimm_label`, etc.
fn try_read_csrow_api(mc_path: &Path, mc_index: usize, dimms: &mut Vec<DimmEdac>) {
    let Some(entries) = sorted_dir_entries(mc_path) else {
        return;
    };

    let mut dimm_counter = 0usize;
    for entry in entries {
        let name = entry.file_name();
        let name = name.to_str().unwrap_or("");
        if !name.starts_with("csrow") {
            continue;
        }

        let csrow_path = entry.path();
        // Each csrow can have multiple channels (ch0, ch1, ...)
        for ch in 0..8 {
            let ce_path = csrow_path.join(format!("ch{ch}_ce_count"));
            if !ce_path.exists() {
                break;
            }
            let ce = read_u64_file(&ce_path).unwrap_or(0);
            let ue = read_u64_file(&csrow_path.join("ue_count")).unwrap_or(0);
            let label = read_trimmed(&csrow_path.join(format!("ch{ch}_dimm_label")));

            dimms.push(DimmEdac {
                mc: mc_index,
                dimm_index: dimm_counter,
                label,
                location: Some(format!("{name} channel {ch}")),
                ce_count: ce,
                ue_count: ue,
            });
            dimm_counter += 1;
        }
    }
}

fn read_u64_file(path: &Path) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn read_trimmed(path: &Path) -> Option<String> {
    let s = fs::read_to_string(path).ok()?.trim().to_owned();
    if s.is_empty() { None } else { Some(s) }
}

fn sorted_dir_entries(path: &Path) -> Option<Vec<fs::DirEntry>> {
    let mut entries: Vec<_> = fs::read_dir(path)
        .ok()?
        .filter_map(std::result::Result::ok)
        .collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    Some(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_snapshot(dimms: Vec<(usize, usize, u64, u64)>) -> EdacSnapshot {
        EdacSnapshot {
            dimms: dimms
                .into_iter()
                .map(|(mc, idx, ce, ue)| DimmEdac {
                    mc,
                    dimm_index: idx,
                    label: None,
                    location: None,
                    ce_count: ce,
                    ue_count: ue,
                })
                .collect(),
            timestamp: Instant::now(),
        }
    }

    #[test]
    fn delta_detects_increase() {
        let before = make_snapshot(vec![(0, 0, 5, 0), (0, 1, 0, 0)]);
        let after = make_snapshot(vec![(0, 0, 8, 0), (0, 1, 0, 1)]);
        let deltas = before.delta(&after);
        assert_eq!(deltas.len(), 2);
        assert_eq!(deltas[0].ce_delta, 3);
        assert_eq!(deltas[0].ue_delta, 0);
        assert_eq!(deltas[1].ce_delta, 0);
        assert_eq!(deltas[1].ue_delta, 1);
    }

    #[test]
    fn delta_ignores_unchanged() {
        let before = make_snapshot(vec![(0, 0, 5, 0)]);
        let after = make_snapshot(vec![(0, 0, 5, 0)]);
        let deltas = before.delta(&after);
        assert!(deltas.is_empty());
    }

    #[test]
    fn read_u64_file_valid() {
        let dir = std::env::temp_dir().join("ferrite_test_edac_u64");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test_count");
        std::fs::write(&path, "42\n").unwrap();
        assert_eq!(read_u64_file(&path), Some(42));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_u64_file_invalid() {
        let dir = std::env::temp_dir().join("ferrite_test_edac_u64_bad");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("bad_count");
        std::fs::write(&path, "not_a_number\n").unwrap();
        assert_eq!(read_u64_file(&path), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_u64_file_missing() {
        assert_eq!(read_u64_file(Path::new("/nonexistent/path")), None);
    }

    #[test]
    fn read_trimmed_non_empty() {
        let dir = std::env::temp_dir().join("ferrite_test_edac_trim");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("label");
        std::fs::write(&path, "  DIMM_A1  \n").unwrap();
        assert_eq!(read_trimmed(&path), Some("DIMM_A1".to_owned()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_trimmed_empty_returns_none() {
        let dir = std::env::temp_dir().join("ferrite_test_edac_trim_empty");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("label");
        std::fs::write(&path, "  \n").unwrap();
        assert_eq!(read_trimmed(&path), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_returns_none_without_edac() {
        // In CI/test environments, EDAC sysfs typically doesn't exist
        // This exercises the early return path
        let result = EdacSnapshot::capture();
        // Can't assert None (might exist on some machines), but it shouldn't panic
        let _ = result;
    }

    mod try_read_dimm_api_tests {
        use assert2::check;
        use tempfile::TempDir;

        use super::*;

        fn setup_dimm_dir(
            mc_path: &Path,
            idx: usize,
            ce: u64,
            ue: u64,
            label: Option<&str>,
            location: Option<&str>,
        ) {
            let dimm = mc_path.join(format!("dimm{idx}"));
            fs::create_dir_all(&dimm).unwrap();
            fs::write(dimm.join("dimm_ce_count"), format!("{ce}\n")).unwrap();
            fs::write(dimm.join("dimm_ue_count"), format!("{ue}\n")).unwrap();
            if let Some(l) = label {
                fs::write(dimm.join("dimm_label"), format!("{l}\n")).unwrap();
            }
            if let Some(loc) = location {
                fs::write(dimm.join("dimm_location"), format!("{loc}\n")).unwrap();
            }
        }

        #[test]
        fn reads_single_dimm() {
            let tmp = TempDir::new().unwrap();
            let mc = tmp.path().join("mc0");
            fs::create_dir_all(&mc).unwrap();
            setup_dimm_dir(&mc, 0, 5, 1, Some("DIMM_A1"), Some("channel 0 slot 0"));

            let mut dimms = Vec::new();
            let found = try_read_dimm_api(&mc, 0, &mut dimms);
            check!(found);
            check!(dimms.len() == 1);
            check!(dimms[0].mc == 0);
            check!(dimms[0].dimm_index == 0);
            check!(dimms[0].ce_count == 5);
            check!(dimms[0].ue_count == 1);
            check!(dimms[0].label == Some("DIMM_A1".to_owned()));
            check!(dimms[0].location == Some("channel 0 slot 0".to_owned()));
        }

        #[test]
        fn reads_multiple_dimms() {
            let tmp = TempDir::new().unwrap();
            let mc = tmp.path().join("mc0");
            fs::create_dir_all(&mc).unwrap();
            setup_dimm_dir(&mc, 0, 0, 0, None, None);
            setup_dimm_dir(&mc, 1, 3, 0, Some("DIMM_B1"), None);

            let mut dimms = Vec::new();
            try_read_dimm_api(&mc, 0, &mut dimms);
            check!(dimms.len() == 2);
            check!(dimms[0].dimm_index == 0);
            check!(dimms[1].dimm_index == 1);
            check!(dimms[1].ce_count == 3);
        }

        #[test]
        fn skips_non_dimm_entries() {
            let tmp = TempDir::new().unwrap();
            let mc = tmp.path().join("mc0");
            fs::create_dir_all(&mc).unwrap();
            setup_dimm_dir(&mc, 0, 0, 0, None, None);
            // Create a non-dimm directory
            fs::create_dir_all(mc.join("some_other_dir")).unwrap();

            let mut dimms = Vec::new();
            try_read_dimm_api(&mc, 0, &mut dimms);
            check!(dimms.len() == 1);
        }

        #[test]
        fn returns_false_for_empty_mc() {
            let tmp = TempDir::new().unwrap();
            let mc = tmp.path().join("mc0");
            fs::create_dir_all(&mc).unwrap();

            let mut dimms = Vec::new();
            check!(!try_read_dimm_api(&mc, 0, &mut dimms));
        }

        #[test]
        fn missing_count_files_default_to_zero() {
            let tmp = TempDir::new().unwrap();
            let mc = tmp.path().join("mc0");
            let dimm = mc.join("dimm0");
            fs::create_dir_all(&dimm).unwrap();
            // Don't create ce/ue count files

            let mut dimms = Vec::new();
            try_read_dimm_api(&mc, 0, &mut dimms);
            check!(dimms.len() == 1);
            check!(dimms[0].ce_count == 0);
            check!(dimms[0].ue_count == 0);
        }
    }

    mod try_read_csrow_api_tests {
        use assert2::check;
        use tempfile::TempDir;

        use super::*;

        fn setup_csrow(mc_path: &Path, csrow: usize, channels: &[(u64, Option<&str>)], ue: u64) {
            let csrow_path = mc_path.join(format!("csrow{csrow}"));
            fs::create_dir_all(&csrow_path).unwrap();
            fs::write(csrow_path.join("ue_count"), format!("{ue}\n")).unwrap();
            for (ch, (ce, label)) in channels.iter().enumerate() {
                fs::write(
                    csrow_path.join(format!("ch{ch}_ce_count")),
                    format!("{ce}\n"),
                )
                .unwrap();
                if let Some(l) = label {
                    fs::write(
                        csrow_path.join(format!("ch{ch}_dimm_label")),
                        format!("{l}\n"),
                    )
                    .unwrap();
                }
            }
        }

        #[test]
        fn reads_single_csrow_single_channel() {
            let tmp = TempDir::new().unwrap();
            let mc = tmp.path().join("mc0");
            fs::create_dir_all(&mc).unwrap();
            setup_csrow(&mc, 0, &[(3, Some("DIMM_A1"))], 1);

            let mut dimms = Vec::new();
            try_read_csrow_api(&mc, 0, &mut dimms);
            check!(dimms.len() == 1);
            check!(dimms[0].mc == 0);
            check!(dimms[0].ce_count == 3);
            check!(dimms[0].ue_count == 1);
            check!(dimms[0].label == Some("DIMM_A1".to_owned()));
            check!(dimms[0].location == Some("csrow0 channel 0".to_owned()));
        }

        #[test]
        fn reads_multi_channel_csrow() {
            let tmp = TempDir::new().unwrap();
            let mc = tmp.path().join("mc0");
            fs::create_dir_all(&mc).unwrap();
            setup_csrow(&mc, 0, &[(1, None), (2, Some("CH_B"))], 0);

            let mut dimms = Vec::new();
            try_read_csrow_api(&mc, 0, &mut dimms);
            check!(dimms.len() == 2);
            check!(dimms[0].ce_count == 1);
            check!(dimms[0].location == Some("csrow0 channel 0".to_owned()));
            check!(dimms[1].ce_count == 2);
            check!(dimms[1].label == Some("CH_B".to_owned()));
        }

        #[test]
        fn skips_non_csrow_entries() {
            let tmp = TempDir::new().unwrap();
            let mc = tmp.path().join("mc0");
            fs::create_dir_all(&mc).unwrap();
            setup_csrow(&mc, 0, &[(0, None)], 0);
            fs::create_dir_all(mc.join("not_a_csrow")).unwrap();

            let mut dimms = Vec::new();
            try_read_csrow_api(&mc, 0, &mut dimms);
            check!(dimms.len() == 1);
        }

        #[test]
        fn empty_mc_produces_no_dimms() {
            let tmp = TempDir::new().unwrap();
            let mc = tmp.path().join("mc0");
            fs::create_dir_all(&mc).unwrap();

            let mut dimms = Vec::new();
            try_read_csrow_api(&mc, 0, &mut dimms);
            check!(dimms.is_empty());
        }
    }

    #[test]
    fn ecc_delta_serialization() {
        let delta_with_label = EccDelta {
            mc: 0,
            dimm_index: 1,
            label: Some("DIMM_A1".to_owned()),
            ce_delta: 3,
            ue_delta: 0,
        };
        let json = serde_json::to_string(&delta_with_label).unwrap();
        assert!(json.contains("\"label\":\"DIMM_A1\""));

        let delta_no_label = EccDelta {
            mc: 0,
            dimm_index: 0,
            label: None,
            ce_delta: 0,
            ue_delta: 1,
        };
        let json = serde_json::to_string(&delta_no_label).unwrap();
        // label should be skipped entirely due to skip_serializing_if
        assert!(!json.contains("label"));
    }

    #[test]
    fn delta_handles_missing_dimm() {
        let before = make_snapshot(vec![(0, 0, 5, 0)]);
        let after = make_snapshot(vec![(0, 0, 6, 0), (0, 1, 1, 0)]);
        let deltas = before.delta(&after);
        // Only mc0/dimm0 has a match; dimm1 is new and has no before counterpart
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].dimm_index, 0);
    }
}
