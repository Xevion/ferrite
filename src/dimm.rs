use std::fmt;

use crate::edac::{DimmEdac, EdacSnapshot};
use crate::smbios::{self, DimmInfo};

/// A merged DIMM entry combining EDAC and SMBIOS data.
#[derive(Debug, Clone)]
pub struct DimmEntry {
    pub edac: Option<DimmEdac>,
    pub smbios: Option<DimmInfo>,
}

/// Unified DIMM topology from EDAC sysfs + SMBIOS Type 17.
#[derive(Debug)]
pub struct DimmTopology {
    pub dimms: Vec<DimmEntry>,
}

impl DimmTopology {
    /// Build topology by merging EDAC and SMBIOS data.
    /// Returns `None` if neither source provides data.
    #[must_use]
    pub fn build() -> Option<Self> {
        let edac = EdacSnapshot::capture();
        let smbios_dimms = smbios::read_dimm_info();
        Self::merge(edac, smbios_dimms)
    }

    /// Merge EDAC and SMBIOS data into a unified topology.
    /// Returns `None` if neither source provides data or the result is empty.
    #[must_use]
    pub(crate) fn merge(
        edac: Option<EdacSnapshot>,
        smbios_dimms: Option<Vec<DimmInfo>>,
    ) -> Option<Self> {
        if edac.is_none() && smbios_dimms.is_none() {
            return None;
        }

        let mut entries = Vec::new();

        match (edac, smbios_dimms) {
            (Some(edac), Some(smbios)) => {
                // Try to match EDAC entries to SMBIOS entries by label/location heuristics
                let mut used_smbios = vec![false; smbios.len()];

                for edac_dimm in &edac.dimms {
                    let matched = find_smbios_match(edac_dimm, &smbios, &used_smbios);
                    if let Some(idx) = matched {
                        used_smbios[idx] = true;
                        entries.push(DimmEntry {
                            edac: Some(edac_dimm.clone()),
                            smbios: Some(smbios[idx].clone()),
                        });
                    } else {
                        entries.push(DimmEntry {
                            edac: Some(edac_dimm.clone()),
                            smbios: None,
                        });
                    }
                }

                // Add any unmatched SMBIOS entries
                for (i, info) in smbios.iter().enumerate() {
                    if !used_smbios[i] {
                        entries.push(DimmEntry {
                            edac: None,
                            smbios: Some(info.clone()),
                        });
                    }
                }
            }
            (Some(edac), None) => {
                for dimm in &edac.dimms {
                    entries.push(DimmEntry {
                        edac: Some(dimm.clone()),
                        smbios: None,
                    });
                }
            }
            (None, Some(smbios)) => {
                for info in &smbios {
                    entries.push(DimmEntry {
                        edac: None,
                        smbios: Some(info.clone()),
                    });
                }
            }
            (None, None) => unreachable!(),
        }

        if entries.is_empty() {
            None
        } else {
            Some(Self { dimms: entries })
        }
    }
}

impl fmt::Display for DimmEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.smbios, &self.edac) {
            (Some(smbios), Some(edac)) => {
                write!(f, "{}", smbios.device_locator)?;
                if let Some(mfr) = &smbios.manufacturer {
                    write!(f, " ({mfr}")?;
                    write!(f, " {}MB", smbios.size_mb)?;
                    write!(f, " {}", smbios.memory_type)?;
                    write!(f, "-{}", smbios.speed_mhz)?;
                    write!(f, ")")?;
                }
                if let Some(loc) = &edac.location {
                    write!(f, " [{loc}]")?;
                }
            }
            (Some(smbios), None) => {
                write!(f, "{}", smbios.device_locator)?;
                if let Some(mfr) = &smbios.manufacturer {
                    write!(f, " ({mfr} {}MB {})", smbios.size_mb, smbios.memory_type)?;
                }
            }
            (None, Some(edac)) => {
                write!(f, "mc{}/dimm{}", edac.mc, edac.dimm_index)?;
                if let Some(loc) = &edac.location {
                    write!(f, " [{loc}]")?;
                }
                if let Some(label) = &edac.label {
                    write!(f, " ({label})")?;
                }
            }
            (None, None) => write!(f, "(unknown)")?,
        }
        Ok(())
    }
}

/// Heuristically match an EDAC DIMM entry to a SMBIOS entry.
///
/// Matching strategies (in priority order):
/// 1. EDAC `label` matches SMBIOS `device_locator` exactly
/// 2. EDAC `location` contains channel/slot info that matches SMBIOS `bank_locator`
fn find_smbios_match(edac: &DimmEdac, smbios: &[DimmInfo], used: &[bool]) -> Option<usize> {
    // Strategy 1: exact label match
    if let Some(label) = &edac.label {
        for (i, info) in smbios.iter().enumerate() {
            if !used[i] && info.device_locator == *label {
                return Some(i);
            }
        }
    }

    // Strategy 2: location string substring match against bank_locator
    if let Some(location) = &edac.location {
        let loc_lower = location.to_lowercase();
        for (i, info) in smbios.iter().enumerate() {
            if !used[i] {
                let bank_lower = info.bank_locator.to_lowercase();
                // Check for shared channel/slot keywords
                if !bank_lower.is_empty()
                    && (bank_lower.contains(&loc_lower) || loc_lower.contains(&bank_lower))
                {
                    return Some(i);
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;
    use crate::smbios::MemoryType;

    fn edac_dimm(mc: usize, idx: usize, label: Option<&str>, location: Option<&str>) -> DimmEdac {
        DimmEdac {
            mc,
            dimm_index: idx,
            label: label.map(str::to_owned),
            location: location.map(str::to_owned),
            ce_count: 0,
            ue_count: 0,
        }
    }

    fn smbios_dimm(device_locator: &str, bank_locator: &str) -> DimmInfo {
        DimmInfo {
            handle: 0,
            device_locator: device_locator.to_owned(),
            bank_locator: bank_locator.to_owned(),
            manufacturer: Some("TestMfr".to_owned()),
            serial_number: None,
            part_number: None,
            size_mb: 8192,
            memory_type: MemoryType::Ddr5,
            speed_mhz: 4800,
        }
    }

    #[test]
    fn match_by_label() {
        let edac = edac_dimm(0, 0, Some("DIMM_A1"), None);
        let smbios = vec![
            smbios_dimm("DIMM_B1", "BANK 1"),
            smbios_dimm("DIMM_A1", "BANK 0"),
        ];
        let used = vec![false, false];
        check!(find_smbios_match(&edac, &smbios, &used) == Some(1));
    }

    #[test]
    fn match_by_location_substring() {
        let edac = edac_dimm(0, 0, None, Some("channel 0 slot 0"));
        let smbios = vec![
            smbios_dimm("DIMM_B1", "P0_Node0_Channel1_Dimm0"),
            smbios_dimm("DIMM_A1", "P0_Node0_Channel0_Dimm0"),
        ];
        let used = vec![false, false];
        // "channel 0 slot 0" doesn't substring-match "P0_Node0_Channel0_Dimm0"
        // This is a known limitation -- exact substring matching is imperfect
        check!(find_smbios_match(&edac, &smbios, &used) == None);
    }

    #[test]
    fn no_match_returns_none() {
        let edac = edac_dimm(0, 0, None, None);
        let smbios = vec![smbios_dimm("DIMM_A1", "BANK 0")];
        let used = vec![false];
        check!(find_smbios_match(&edac, &smbios, &used) == None);
    }

    #[test]
    fn display_full_entry() {
        let entry = DimmEntry {
            edac: Some(edac_dimm(0, 0, None, Some("channel 0 slot 0"))),
            smbios: Some(smbios_dimm("DIMM_A1", "BANK 0")),
        };
        let s = entry.to_string();
        assert!(s.contains("DIMM_A1"));
        assert!(s.contains("TestMfr"));
        assert!(s.contains("channel 0 slot 0"));
    }

    #[test]
    fn display_smbios_only_with_manufacturer() {
        let entry = DimmEntry {
            edac: None,
            smbios: Some(smbios_dimm("DIMM_B1", "BANK 1")),
        };
        let s = entry.to_string();
        assert!(s.contains("DIMM_B1"));
        assert!(s.contains("TestMfr"));
        assert!(s.contains("8192MB"));
    }

    #[test]
    fn display_smbios_only_without_manufacturer() {
        let mut info = smbios_dimm("DIMM_C1", "BANK 2");
        info.manufacturer = None;
        let entry = DimmEntry {
            edac: None,
            smbios: Some(info),
        };
        let s = entry.to_string();
        check!(s == "DIMM_C1");
    }

    #[test]
    fn display_edac_only_with_location_and_label() {
        let entry = DimmEntry {
            edac: Some(edac_dimm(1, 2, Some("DIMM_X"), Some("channel 1 slot 0"))),
            smbios: None,
        };
        let s = entry.to_string();
        assert!(s.contains("mc1/dimm2"));
        assert!(s.contains("channel 1 slot 0"));
        assert!(s.contains("DIMM_X"));
    }

    #[test]
    fn display_edac_only_no_label_no_location() {
        let entry = DimmEntry {
            edac: Some(edac_dimm(0, 3, None, None)),
            smbios: None,
        };
        check!(entry.to_string() == "mc0/dimm3");
    }

    #[test]
    fn display_neither() {
        let entry = DimmEntry {
            edac: None,
            smbios: None,
        };
        check!(entry.to_string() == "(unknown)");
    }

    mod merge_tests {
        use std::time::Instant;

        use assert2::{assert, check};

        use super::*;
        use crate::edac::EdacSnapshot;

        fn make_edac(dimms: Vec<DimmEdac>) -> EdacSnapshot {
            EdacSnapshot {
                dimms,
                timestamp: Instant::now(),
            }
        }

        #[test]
        fn none_none_returns_none() {
            check!(DimmTopology::merge(None, None).is_none());
        }

        #[test]
        fn edac_only() {
            let edac = make_edac(vec![edac_dimm(0, 0, Some("DIMM_A1"), None)]);
            let topo = DimmTopology::merge(Some(edac), None).unwrap();
            check!(topo.dimms.len() == 1);
            assert!(topo.dimms[0].edac.is_some());
            assert!(topo.dimms[0].smbios.is_none());
        }

        #[test]
        fn smbios_only() {
            let smbios = vec![smbios_dimm("DIMM_A1", "BANK 0")];
            let topo = DimmTopology::merge(None, Some(smbios)).unwrap();
            check!(topo.dimms.len() == 1);
            assert!(topo.dimms[0].edac.is_none());
            assert!(topo.dimms[0].smbios.is_some());
        }

        #[test]
        fn matched_merge() {
            let edac = make_edac(vec![edac_dimm(0, 0, Some("DIMM_A1"), None)]);
            let smbios = vec![smbios_dimm("DIMM_A1", "BANK 0")];
            let topo = DimmTopology::merge(Some(edac), Some(smbios)).unwrap();
            check!(topo.dimms.len() == 1);
            assert!(topo.dimms[0].edac.is_some());
            assert!(topo.dimms[0].smbios.is_some());
        }

        #[test]
        fn unmatched_entries_kept() {
            let edac = make_edac(vec![edac_dimm(0, 0, None, None)]);
            let smbios = vec![smbios_dimm("DIMM_A1", "BANK 0")];
            let topo = DimmTopology::merge(Some(edac), Some(smbios)).unwrap();
            // EDAC entry unmatched + SMBIOS entry unmatched = 2 entries
            check!(topo.dimms.len() == 2);
        }

        #[test]
        fn partial_match_preserves_all() {
            let edac = make_edac(vec![
                edac_dimm(0, 0, Some("DIMM_A1"), None),
                edac_dimm(0, 1, None, None),
            ]);
            let smbios = vec![
                smbios_dimm("DIMM_A1", "BANK 0"),
                smbios_dimm("DIMM_B1", "BANK 1"),
            ];
            let topo = DimmTopology::merge(Some(edac), Some(smbios)).unwrap();
            // DIMM_A1 matched, edac dimm1 unmatched, DIMM_B1 unmatched = 3
            check!(topo.dimms.len() == 3);
        }
    }

    #[test]
    fn match_skips_used_slots() {
        let edac = edac_dimm(0, 0, Some("DIMM_A1"), None);
        let smbios = vec![
            smbios_dimm("DIMM_A1", "BANK 0"),
            smbios_dimm("DIMM_A1", "BANK 1"), // duplicate label
        ];
        // First slot is already used
        let used = vec![true, false];
        check!(find_smbios_match(&edac, &smbios, &used) == Some(1));
    }

    #[test]
    fn match_by_bank_locator_substring() {
        let edac = edac_dimm(0, 0, None, Some("BANK 0"));
        let smbios = vec![
            smbios_dimm("DIMM_B1", "BANK 1"),
            smbios_dimm("DIMM_A1", "BANK 0"),
        ];
        let used = vec![false, false];
        check!(find_smbios_match(&edac, &smbios, &used) == Some(1));
    }

    #[test]
    fn display_full_entry_without_manufacturer() {
        let mut info = smbios_dimm("DIMM_A1", "BANK 0");
        info.manufacturer = None;
        let entry = DimmEntry {
            edac: Some(edac_dimm(0, 0, None, Some("channel 0 slot 0"))),
            smbios: Some(info),
        };
        let s = entry.to_string();
        // Should show device_locator and location but no manufacturer block
        assert!(s.contains("DIMM_A1"));
        assert!(s.contains("channel 0 slot 0"));
        assert!(!s.contains("TestMfr"));
    }
}
