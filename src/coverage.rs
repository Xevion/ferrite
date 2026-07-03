//! Cross-run physical coverage persistence.
//!
//! A coverage store accumulates the set of physical frames (PFNs) that
//! completed runs have tested, so consecutive runs -- and runs separated by
//! reboots -- can report cumulative coverage and how much new physical memory
//! each run contributed. See `docs/COVERAGE.md`.
//!
//! On-disk format: versioned JSON with compacted PFN ranges. The store is
//! bound to a machine fingerprint (`MemTotal` + the `/proc/iomem` "System
//! RAM" layout); a mismatch means the memory configuration changed and the
//! old map is meaningless.

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};
use snafu::{ResultExt, Snafu};

/// Bytes per tracked frame (4 KiB pages, matching pagemap granularity).
pub const FRAME_BYTES: u64 = 4096;

/// Current on-disk schema version.
pub const STORE_VERSION: u32 = 1;

/// A contiguous run of physical page frames: `[start, start + count)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PfnRange {
    pub start: u64,
    pub count: u64,
}

/// Identity of the machine's physical memory layout. A coverage store only
/// merges runs taken under an identical fingerprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    /// `MemTotal` in bytes.
    pub mem_total: u64,
    /// FNV-1a hash of the `/proc/iomem` "System RAM" `(start, end)` ranges.
    pub iomem_hash: u64,
}

/// One completed run merged into the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    pub timestamp: jiff::Timestamp,
    pub patterns: Vec<String>,
    pub passes: usize,
    pub tested_bytes: u64,
    pub new_bytes: u64,
    pub failures: u64,
}

/// What a run contributed, relative to the store's prior contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunDelta {
    pub new_bytes: u64,
    pub cumulative_bytes: u64,
    pub runs: u64,
}

#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum CoverageError {
    #[snafu(display("failed to read coverage file: {source}"))]
    Read { source: io::Error },
    #[snafu(display("failed to write coverage file: {source}"))]
    Write { source: io::Error },
    #[snafu(display("coverage file is not valid JSON: {source}"))]
    Parse { source: serde_json::Error },
    #[snafu(display(
        "coverage file schema version {found} is not supported (expected {STORE_VERSION})"
    ))]
    VersionMismatch { found: u32 },
    #[snafu(display(
        "coverage file belongs to a different memory configuration \
         (fingerprint mismatch) -- delete it or point --coverage-file elsewhere"
    ))]
    FingerprintMismatch,
}

/// Persistent cumulative coverage: every frame any completed run has tested.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageStore {
    pub version: u32,
    pub fingerprint: Fingerprint,
    pub runs: Vec<RunRecord>,
    /// Sorted, disjoint, non-adjacent ranges -- the canonical covered set.
    pub ranges: Vec<PfnRange>,
}

/// Compact a raw PFN list (as produced by pagemap resolution, zero =
/// unresolved) into sorted, disjoint, merged ranges.
#[must_use]
pub fn compact_pfns(pfns: &[u64]) -> Vec<PfnRange> {
    let mut sorted: Vec<u64> = pfns.iter().copied().filter(|&p| p != 0).collect();
    sorted.sort_unstable();
    sorted.dedup();

    let mut ranges: Vec<PfnRange> = Vec::new();
    for pfn in sorted {
        match ranges.last_mut() {
            Some(last) if last.start + last.count == pfn => last.count += 1,
            _ => ranges.push(PfnRange {
                start: pfn,
                count: 1,
            }),
        }
    }
    ranges
}

/// Union `add` into `base` (both sorted/disjoint), returning the merged
/// ranges and how many frames of `add` were not already present.
#[must_use]
pub fn merge_ranges(base: &[PfnRange], add: &[PfnRange]) -> (Vec<PfnRange>, u64) {
    // Sweep both sorted lists together, coalescing overlap and adjacency.
    let mut all: Vec<PfnRange> = base.iter().chain(add.iter()).copied().collect();
    all.sort_unstable_by_key(|r| r.start);

    let mut merged: Vec<PfnRange> = Vec::with_capacity(all.len());
    for range in all {
        match merged.last_mut() {
            Some(last) if range.start <= last.start + last.count => {
                let end = (range.start + range.count).max(last.start + last.count);
                last.count = end - last.start;
            }
            _ => merged.push(range),
        }
    }

    let new_frames = total_frames(&merged) - total_frames(base);
    (merged, new_frames)
}

/// Total frames covered by a compacted range list.
#[must_use]
pub fn total_frames(ranges: &[PfnRange]) -> u64 {
    ranges.iter().map(|r| r.count).sum()
}

/// The frames of `universe` not present in `covered` (both sorted/disjoint).
#[must_use]
pub fn subtract_ranges(universe: &[PfnRange], covered: &[PfnRange]) -> Vec<PfnRange> {
    let mut out = Vec::new();
    let mut cov = covered.iter().copied().peekable();
    for u in universe {
        let mut start = u.start;
        let end = u.start + u.count;
        while start < end {
            // Discard covered ranges that end at or before the cursor.
            while cov.peek().is_some_and(|c| c.start + c.count <= start) {
                cov.next();
            }
            match cov.peek() {
                Some(c) if c.start < end => {
                    if c.start > start {
                        out.push(PfnRange {
                            start,
                            count: c.start - start,
                        });
                    }
                    // Do not consume: a covered range may span several
                    // universe ranges.
                    start = c.start + c.count;
                }
                _ => {
                    out.push(PfnRange {
                        start,
                        count: end - start,
                    });
                    start = end;
                }
            }
        }
    }
    out
}

/// Whether `pfn` falls inside any of the sorted/disjoint `ranges`.
#[inline]
#[must_use]
pub fn contains_pfn(ranges: &[PfnRange], pfn: u64) -> bool {
    ranges
        .binary_search_by(|r| {
            if pfn < r.start {
                std::cmp::Ordering::Greater
            } else if pfn >= r.start + r.count {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// FNV-1a over the System RAM `(start, end)` pairs, mixing `mem_total` in.
#[must_use]
pub fn fingerprint_from(mem_total: u64, system_ram: &[(u64, u64)]) -> Fingerprint {
    // FNV-1a: stable across builds and Rust versions, unlike DefaultHasher.
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = FNV_OFFSET;
    let mut mix = |value: u64| {
        for byte in value.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    };
    for &(start, end) in system_ram {
        mix(start);
        mix(end);
    }

    Fingerprint {
        mem_total,
        iomem_hash: hash,
    }
}

impl CoverageStore {
    /// An empty store bound to `fingerprint`.
    #[must_use]
    pub const fn new(fingerprint: Fingerprint) -> Self {
        Self {
            version: STORE_VERSION,
            fingerprint,
            runs: Vec::new(),
            ranges: Vec::new(),
        }
    }

    /// Load a store from `path`. Returns `Ok(None)` when the file does not
    /// exist. Fails on unreadable/invalid files, unsupported schema versions,
    /// and fingerprint mismatches.
    ///
    /// # Errors
    ///
    /// See [`CoverageError`].
    pub fn load(path: &Path, fingerprint: Fingerprint) -> Result<Option<Self>, CoverageError> {
        let contents = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(CoverageError::Read { source: e }),
        };
        let store: Self = serde_json::from_str(&contents).context(ParseSnafu)?;
        if store.version != STORE_VERSION {
            return Err(CoverageError::VersionMismatch {
                found: store.version,
            });
        }
        if store.fingerprint != fingerprint {
            return Err(CoverageError::FingerprintMismatch);
        }
        Ok(Some(store))
    }

    /// Atomically persist the store to `path` (write temp + rename).
    ///
    /// # Errors
    ///
    /// Returns [`CoverageError::Write`] on any I/O failure.
    pub fn save(&self, path: &Path) -> Result<(), CoverageError> {
        let json = serde_json::to_string(self).map_err(|e| CoverageError::Write {
            source: io::Error::other(e),
        })?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).context(WriteSnafu)?;
        std::fs::rename(&tmp, path).context(WriteSnafu)
    }

    /// Merge a completed run's tested ranges into the store and append a run
    /// record. Returns the delta this run contributed.
    pub fn record_run(
        &mut self,
        tested: &[PfnRange],
        timestamp: jiff::Timestamp,
        patterns: Vec<String>,
        passes: usize,
        failures: u64,
    ) -> RunDelta {
        let (merged, new_frames) = merge_ranges(&self.ranges, tested);
        self.ranges = merged;
        let new_bytes = new_frames * FRAME_BYTES;
        self.runs.push(RunRecord {
            timestamp,
            patterns,
            passes,
            tested_bytes: total_frames(tested) * FRAME_BYTES,
            new_bytes,
            failures,
        });
        RunDelta {
            new_bytes,
            cumulative_bytes: self.covered_bytes(),
            runs: self.runs.len() as u64,
        }
    }

    /// Cumulative covered bytes.
    #[must_use]
    pub fn covered_bytes(&self) -> u64 {
        total_frames(&self.ranges) * FRAME_BYTES
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(start: u64, count: u64) -> PfnRange {
        PfnRange { start, count }
    }

    mod compact {
        use assert2::check;

        use super::*;

        #[test]
        fn empty_input_is_empty() {
            check!(compact_pfns(&[]) == vec![]);
        }

        #[test]
        fn zero_pfns_are_dropped() {
            check!(compact_pfns(&[0, 0, 0]) == vec![]);
        }

        #[test]
        fn contiguous_pfns_merge_into_one_range() {
            check!(compact_pfns(&[5, 6, 7]) == vec![r(5, 3)]);
        }

        #[test]
        fn unsorted_input_is_sorted_first() {
            check!(compact_pfns(&[7, 5, 6]) == vec![r(5, 3)]);
        }

        #[test]
        fn duplicates_collapse() {
            check!(compact_pfns(&[5, 5, 6, 6]) == vec![r(5, 2)]);
        }

        #[test]
        fn gaps_split_ranges() {
            check!(compact_pfns(&[1, 2, 10, 11, 20]) == vec![r(1, 2), r(10, 2), r(20, 1)]);
        }
    }

    mod merge {
        use assert2::check;

        use super::*;

        #[test]
        fn merge_into_empty_base_is_identity() {
            let (merged, new) = merge_ranges(&[], &[r(5, 3)]);
            check!(merged == vec![r(5, 3)]);
            check!(new == 3);
        }

        #[test]
        fn disjoint_ranges_interleave_sorted() {
            let (merged, new) = merge_ranges(&[r(10, 2)], &[r(1, 2), r(20, 1)]);
            check!(merged == vec![r(1, 2), r(10, 2), r(20, 1)]);
            check!(new == 3);
        }

        #[test]
        fn full_overlap_adds_nothing() {
            let (merged, new) = merge_ranges(&[r(5, 10)], &[r(6, 3)]);
            check!(merged == vec![r(5, 10)]);
            check!(new == 0);
        }

        #[test]
        fn partial_overlap_counts_only_new_frames() {
            // base [5,10), add [8,13) -> merged [5,13), 3 new frames (10,11,12)
            let (merged, new) = merge_ranges(&[r(5, 5)], &[r(8, 5)]);
            check!(merged == vec![r(5, 8)]);
            check!(new == 3);
        }

        #[test]
        fn adjacent_ranges_coalesce() {
            let (merged, new) = merge_ranges(&[r(5, 3)], &[r(8, 2)]);
            check!(merged == vec![r(5, 5)]);
            check!(new == 2);
        }

        #[test]
        fn add_bridging_two_base_ranges_coalesces_all() {
            // base [1,3) and [6,8); add [3,6) bridges them.
            let (merged, new) = merge_ranges(&[r(1, 2), r(6, 2)], &[r(3, 3)]);
            check!(merged == vec![r(1, 7)]);
            check!(new == 3);
        }
    }

    mod subtract {
        use assert2::check;

        use super::*;

        #[test]
        fn empty_covered_returns_universe() {
            check!(subtract_ranges(&[r(5, 10)], &[]) == vec![r(5, 10)]);
        }

        #[test]
        fn empty_universe_is_empty() {
            check!(subtract_ranges(&[], &[r(5, 10)]) == vec![]);
        }

        #[test]
        fn full_cover_is_empty() {
            check!(subtract_ranges(&[r(5, 10)], &[r(5, 10)]) == vec![]);
        }

        #[test]
        fn cover_exceeding_universe_is_empty() {
            check!(subtract_ranges(&[r(5, 10)], &[r(0, 100)]) == vec![]);
        }

        #[test]
        fn middle_cover_splits_universe() {
            // universe [0,10), covered [3,6) -> [0,3) and [6,10)
            check!(subtract_ranges(&[r(0, 10)], &[r(3, 3)]) == vec![r(0, 3), r(6, 4)]);
        }

        #[test]
        fn front_cover_trims_start() {
            check!(subtract_ranges(&[r(0, 10)], &[r(0, 4)]) == vec![r(4, 6)]);
        }

        #[test]
        fn back_cover_trims_end() {
            check!(subtract_ranges(&[r(0, 10)], &[r(7, 3)]) == vec![r(0, 7)]);
        }

        #[test]
        fn covered_outside_universe_is_ignored() {
            check!(subtract_ranges(&[r(10, 5)], &[r(0, 5), r(20, 5)]) == vec![r(10, 5)]);
        }

        #[test]
        fn one_cover_spans_multiple_universe_ranges() {
            // covered [3,25) blankets the tail of [0,10) and all of [12,20),
            // and trims the head of [22,30).
            let universe = [r(0, 10), r(12, 8), r(22, 8)];
            check!(subtract_ranges(&universe, &[r(3, 22)]) == vec![r(0, 3), r(25, 5)]);
        }

        #[test]
        fn multiple_covers_within_one_universe_range() {
            let covered = [r(2, 2), r(6, 2)];
            check!(subtract_ranges(&[r(0, 10)], &covered) == vec![r(0, 2), r(4, 2), r(8, 2)]);
        }
    }

    mod contains {
        use assert2::check;

        use super::*;

        #[test]
        fn hits_within_ranges() {
            let ranges = [r(5, 3), r(20, 2)];
            check!(contains_pfn(&ranges, 5));
            check!(contains_pfn(&ranges, 7));
            check!(contains_pfn(&ranges, 20));
            check!(contains_pfn(&ranges, 21));
        }

        #[test]
        fn misses_boundaries_and_gaps() {
            let ranges = [r(5, 3), r(20, 2)];
            check!(!contains_pfn(&ranges, 4));
            check!(!contains_pfn(&ranges, 8));
            check!(!contains_pfn(&ranges, 19));
            check!(!contains_pfn(&ranges, 22));
        }

        #[test]
        fn empty_ranges_contain_nothing() {
            check!(!contains_pfn(&[], 0));
        }
    }

    mod fingerprints {
        use assert2::check;

        use super::*;

        #[test]
        fn identical_inputs_agree() {
            let a = fingerprint_from(32, &[(0x1000, 0x2000), (0x5000, 0x9000)]);
            let b = fingerprint_from(32, &[(0x1000, 0x2000), (0x5000, 0x9000)]);
            check!(a == b);
        }

        #[test]
        fn different_layout_differs() {
            let a = fingerprint_from(32, &[(0x1000, 0x2000)]);
            let b = fingerprint_from(32, &[(0x1000, 0x3000)]);
            check!(a.iomem_hash != b.iomem_hash);
        }

        #[test]
        fn mem_total_is_carried() {
            let a = fingerprint_from(42, &[]);
            check!(a.mem_total == 42);
        }
    }

    mod store {
        use assert2::{assert, check};

        use super::*;

        fn fp() -> Fingerprint {
            fingerprint_from(32_000_000_000, &[(0x1000, 0xffff_ffff)])
        }

        fn ts() -> jiff::Timestamp {
            jiff::Timestamp::UNIX_EPOCH
        }

        #[test]
        fn record_run_merges_and_logs() {
            let mut store = CoverageStore::new(fp());
            let delta = store.record_run(&[r(5, 5)], ts(), vec!["solid-bits".into()], 1, 0);
            check!(delta.new_bytes == 5 * FRAME_BYTES);
            check!(delta.cumulative_bytes == 5 * FRAME_BYTES);
            check!(delta.runs == 1);

            let delta2 = store.record_run(&[r(8, 5)], ts(), vec!["solid-bits".into()], 1, 3);
            check!(delta2.new_bytes == 3 * FRAME_BYTES);
            check!(delta2.cumulative_bytes == 8 * FRAME_BYTES);
            check!(delta2.runs == 2);

            check!(store.runs.len() == 2);
            check!(store.runs[1].failures == 3);
            check!(store.runs[1].new_bytes == 3 * FRAME_BYTES);
            check!(store.runs[1].tested_bytes == 5 * FRAME_BYTES);
        }

        #[test]
        fn save_load_roundtrip() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("coverage.json");

            let mut store = CoverageStore::new(fp());
            store.record_run(&[r(5, 5)], ts(), vec!["checkerboard".into()], 2, 0);
            store.save(&path).unwrap();

            let loaded = CoverageStore::load(&path, fp()).unwrap().unwrap();
            check!(loaded == store);
        }

        #[test]
        fn missing_file_loads_none() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("nope.json");
            let loaded = CoverageStore::load(&path, fp()).unwrap();
            check!(loaded.is_none());
        }

        #[test]
        fn fingerprint_mismatch_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("coverage.json");
            CoverageStore::new(fp()).save(&path).unwrap();

            let other = fingerprint_from(16_000_000_000, &[(0x1000, 0xffff)]);
            let Err(e) = CoverageStore::load(&path, other) else {
                panic!("expected fingerprint mismatch");
            };
            assert!(let CoverageError::FingerprintMismatch = e);
        }

        #[test]
        fn unsupported_version_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("coverage.json");
            let mut store = CoverageStore::new(fp());
            store.version = 999;
            store.save(&path).unwrap();

            let Err(e) = CoverageStore::load(&path, fp()) else {
                panic!("expected version mismatch");
            };
            assert!(let CoverageError::VersionMismatch { found: 999 } = e);
        }

        #[test]
        fn corrupt_json_is_parse_error() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("coverage.json");
            std::fs::write(&path, "{not json").unwrap();

            let Err(e) = CoverageStore::load(&path, fp()) else {
                panic!("expected parse error");
            };
            assert!(let CoverageError::Parse { .. } = e);
        }
    }
}
