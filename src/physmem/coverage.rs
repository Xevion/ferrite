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

use super::PAGE_BYTES;
use super::pfn::{PfnRange, merge_ranges, total_frames};

/// Current on-disk schema version.
pub const STORE_VERSION: u32 = 1;

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
        let new_bytes = new_frames * PAGE_BYTES;
        self.runs.push(RunRecord {
            timestamp,
            patterns,
            passes,
            tested_bytes: total_frames(tested) * PAGE_BYTES,
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
        total_frames(&self.ranges) * PAGE_BYTES
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::physmem::pfn::Pfn;

    fn r(start: u64, count: u64) -> PfnRange {
        PfnRange {
            start: Pfn(start),
            count,
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
            check!(delta.new_bytes == 5 * PAGE_BYTES);
            check!(delta.cumulative_bytes == 5 * PAGE_BYTES);
            check!(delta.runs == 1);

            let delta2 = store.record_run(&[r(8, 5)], ts(), vec!["solid-bits".into()], 1, 3);
            check!(delta2.new_bytes == 3 * PAGE_BYTES);
            check!(delta2.cumulative_bytes == 8 * PAGE_BYTES);
            check!(delta2.runs == 2);

            check!(store.runs.len() == 2);
            check!(store.runs[1].failures == 3);
            check!(store.runs[1].new_bytes == 3 * PAGE_BYTES);
            check!(store.runs[1].tested_bytes == 5 * PAGE_BYTES);
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
        fn load_dir_path_returns_read_error() {
            let dir = tempfile::tempdir().unwrap();

            let Err(e) = CoverageStore::load(dir.path(), fp()) else {
                panic!("expected read error");
            };
            assert!(let CoverageError::Read { .. } = e);
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
