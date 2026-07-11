//! Gap classification: explaining the untested remainder of installed RAM.
//!
//! "Coverage: 81%" hides whether the other 19% is reachable at all. This
//! module scans `/proc/kpageflags` (indexed by PFN, root-readable) for every
//! System RAM frame outside the covered set and buckets it by what userspace
//! could do about it: acquirable right now, reclaimable under pressure, in
//! use by other processes, or unreachable by design (kernel text, slab,
//! reserved). See `docs/COVERAGE.md` ("Honest denominators") and XEV-1019.

use serde::Serialize;
use tracing::warn;

use crate::physmem::PAGE_BYTES;
use crate::physmem::kpageflags::{self, KPageFlags, READ_BATCH_FRAMES};
use crate::physmem::pfn::{Pfn, PfnRange, subtract_ranges};

/// What an untested physical frame is doing, and therefore whether another
/// run could reach it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameClass {
    /// In the buddy allocator -- acquirable right now.
    Free,
    /// File-backed page cache -- acquirable under allocation pressure.
    Reclaimable,
    /// Anonymous or shmem/tmpfs memory of other processes -- freed by
    /// stopping services or rebooting.
    InUse,
    /// Kernel text/data, slab, page tables, reserved, poisoned, offline --
    /// unreachable from userspace by design.
    Unreachable,
}

/// Classify one frame's kpageflags.
#[must_use]
pub const fn classify(flags: KPageFlags) -> FrameClass {
    if flags.intersects(
        KPageFlags::NOPAGE
            .union(KPageFlags::OFFLINE)
            .union(KPageFlags::HWPOISON)
            .union(KPageFlags::SLAB)
            .union(KPageFlags::PGTABLE),
    ) {
        FrameClass::Unreachable
    } else if flags.contains(KPageFlags::BUDDY) {
        FrameClass::Free
    } else if flags.intersects(KPageFlags::ANON.union(KPageFlags::SWAPBACKED)) {
        FrameClass::InUse
    } else if flags.contains(KPageFlags::LRU) {
        FrameClass::Reclaimable
    } else {
        // Kernel text/data, reserved, driver pages: no public flags.
        FrameClass::Unreachable
    }
}

/// Byte totals per [`FrameClass`] over the untested remainder.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct GapReport {
    /// In the buddy allocator -- acquirable right now.
    pub free_bytes: u64,
    /// File-backed page cache -- acquirable under allocation pressure.
    pub reclaimable_bytes: u64,
    /// In use by other processes (anonymous or shmem/tmpfs).
    pub in_use_bytes: u64,
    /// Unreachable from userspace by design (kernel text/data, slab, reserved, etc.).
    pub unreachable_bytes: u64,
    /// Frames whose flags could not be read.
    #[serde(skip_serializing_if = "is_zero")]
    pub unknown_bytes: u64,
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "signature fixed by serde's skip_serializing_if contract"
)]
const fn is_zero(v: &u64) -> bool {
    *v == 0
}

impl GapReport {
    /// Total untested bytes across all classes.
    #[must_use]
    pub const fn total_bytes(&self) -> u64 {
        self.free_bytes
            + self.reclaimable_bytes
            + self.in_use_bytes
            + self.unreachable_bytes
            + self.unknown_bytes
    }

    const fn add(&mut self, class: FrameClass) {
        let bucket = match class {
            FrameClass::Free => &mut self.free_bytes,
            FrameClass::Reclaimable => &mut self.reclaimable_bytes,
            FrameClass::InUse => &mut self.in_use_bytes,
            FrameClass::Unreachable => &mut self.unreachable_bytes,
        };
        *bucket += PAGE_BYTES;
    }
}

/// Convert inclusive System RAM byte ranges (as parsed from `/proc/iomem`)
/// into the PFN ranges they fully contain.
#[must_use]
pub fn ram_pfn_ranges(ranges: &[(u64, u64)]) -> Vec<PfnRange> {
    ranges
        .iter()
        .filter_map(|&(start, end)| {
            let first = start.div_ceil(PAGE_BYTES);
            let last = (end + 1) / PAGE_BYTES;
            (last > first).then(|| PfnRange {
                start: Pfn::new(first),
                count: last - first,
            })
        })
        .collect()
}

/// Classify every frame in `gaps`, reading raw kpageflags in batches via `read`.
///
/// `read(range, out)` must fill `out` (length `range.count`) with the flags
/// for `range` and return `true`; `false` counts the batch as unknown.
pub fn classify_gaps(
    gaps: &[PfnRange],
    read: &mut dyn FnMut(PfnRange, &mut [u64]) -> bool,
) -> GapReport {
    let mut report = GapReport::default();
    let mut buf = vec![0u64; READ_BATCH_FRAMES as usize];
    for gap in gaps {
        let mut offset = 0;
        while offset < gap.count {
            let count = READ_BATCH_FRAMES.min(gap.count - offset);
            let range = PfnRange {
                start: gap.start + offset,
                count,
            };
            // `count` is capped at READ_BATCH_FRAMES, far below usize::MAX.
            let out = &mut buf[..count as usize];
            if read(range, out) {
                for &word in &*out {
                    report.add(classify(KPageFlags::from_bits_retain(word)));
                }
            } else {
                report.unknown_bytes += count * PAGE_BYTES;
            }
            offset += count;
        }
    }
    report
}

/// Scan the live system: classify every System RAM frame outside `covered`.
///
/// `covered` is sorted/disjoint PFN ranges. Returns `None` when `/proc/iomem`
/// reads as zeroed (non-root) or `/proc/kpageflags` cannot be opened.
#[cfg_attr(coverage_nightly, coverage(off))]
#[must_use]
pub fn classify_system_gaps(covered: &[PfnRange]) -> Option<GapReport> {
    let iomem = match std::fs::read_to_string("/proc/iomem") {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            warn!(path = "/proc/iomem", error = %e, "failed to read for gap classification");
            return None;
        }
    };
    let universe = ram_pfn_ranges(&crate::physmem::sysmem::system_ram_ranges(&iomem));
    if universe.is_empty() {
        return None;
    }
    // Permission denied here (the common non-root case) is the interesting
    // signal: it explains why the gap breakdown is unavailable.
    let file = match std::fs::File::open("/proc/kpageflags") {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            warn!(path = "/proc/kpageflags", error = %e, "failed to open for gap classification");
            return None;
        }
    };
    let gaps = subtract_ranges(&universe, covered);

    let mut scratch = vec![0u8; READ_BATCH_FRAMES as usize * 8];
    Some(classify_gaps(&gaps, &mut |range, out| {
        kpageflags::read_batch(&file, range, &mut scratch, out)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(start: u64, count: u64) -> PfnRange {
        PfnRange {
            start: Pfn::new(start),
            count,
        }
    }

    mod classify_flags {
        use assert2::check;

        use super::*;

        #[test]
        fn buddy_is_free() {
            check!(classify(KPageFlags::BUDDY) == FrameClass::Free);
        }

        #[test]
        fn file_cache_is_reclaimable() {
            // Typical page-cache frame: LRU (+ uptodate/referenced noise).
            check!(classify(KPageFlags::LRU) == FrameClass::Reclaimable);
            check!(
                classify(KPageFlags::from_bits_retain(
                    KPageFlags::LRU.bits() | (1 << 3) | (1 << 2)
                )) == FrameClass::Reclaimable
            );
        }

        #[test]
        fn anon_is_in_use() {
            check!(
                classify(KPageFlags::ANON | KPageFlags::LRU | KPageFlags::SWAPBACKED)
                    == FrameClass::InUse
            );
        }

        #[test]
        fn shmem_without_anon_is_in_use() {
            // tmpfs/shmem: swap-backed but not anonymous.
            check!(classify(KPageFlags::SWAPBACKED | KPageFlags::LRU) == FrameClass::InUse);
        }

        #[test]
        fn slab_and_pgtable_are_unreachable() {
            check!(classify(KPageFlags::SLAB) == FrameClass::Unreachable);
            check!(classify(KPageFlags::PGTABLE) == FrameClass::Unreachable);
        }

        #[test]
        fn nopage_offline_hwpoison_are_unreachable() {
            check!(classify(KPageFlags::NOPAGE) == FrameClass::Unreachable);
            check!(classify(KPageFlags::OFFLINE) == FrameClass::Unreachable);
            check!(classify(KPageFlags::HWPOISON) == FrameClass::Unreachable);
        }

        #[test]
        fn hwpoison_trumps_everything() {
            check!(classify(KPageFlags::HWPOISON | KPageFlags::BUDDY) == FrameClass::Unreachable);
            check!(
                classify(KPageFlags::HWPOISON | KPageFlags::ANON | KPageFlags::LRU)
                    == FrameClass::Unreachable
            );
        }

        #[test]
        fn no_flags_is_unreachable() {
            // Kernel text/data and reserved frames expose no public flags.
            check!(classify(KPageFlags::empty()) == FrameClass::Unreachable);
        }
    }

    mod report {
        use assert2::check;

        use super::*;

        #[test]
        fn total_sums_all_classes() {
            let report = GapReport {
                free_bytes: 1,
                reclaimable_bytes: 2,
                in_use_bytes: 4,
                unreachable_bytes: 8,
                unknown_bytes: 16,
            };
            check!(report.total_bytes() == 31);
        }

        #[test]
        fn serializes_named_fields_and_omits_zero_unknown() {
            let report = GapReport {
                free_bytes: 4096,
                reclaimable_bytes: 8192,
                in_use_bytes: 0,
                unreachable_bytes: 4096,
                unknown_bytes: 0,
            };
            let json = serde_json::to_value(report).unwrap();
            check!(json["free_bytes"] == 4096);
            check!(json["reclaimable_bytes"] == 8192);
            check!(json["in_use_bytes"] == 0);
            check!(json["unreachable_bytes"] == 4096);
            check!(json.get("unknown_bytes") == None);
        }

        #[test]
        fn serializes_nonzero_unknown() {
            let report = GapReport {
                unknown_bytes: 4096,
                ..GapReport::default()
            };
            let json = serde_json::to_value(report).unwrap();
            check!(json["unknown_bytes"] == 4096);
        }
    }

    mod pfn_ranges {
        use assert2::check;

        use super::*;

        #[test]
        fn aligned_range_converts_exactly() {
            // 0x1000-0x8fff inclusive = frames 1..9.
            check!(ram_pfn_ranges(&[(0x1000, 0x8fff)]) == vec![r(1, 8)]);
        }

        #[test]
        fn unaligned_edges_shrink_to_contained_frames() {
            // Start mid-frame: frame 1 is partial, first full frame is 2.
            check!(ram_pfn_ranges(&[(0x1001, 0x8fff)]) == vec![r(2, 7)]);
            // End mid-frame: frame 8 is partial and dropped.
            check!(ram_pfn_ranges(&[(0x1000, 0x8ffe)]) == vec![r(1, 7)]);
        }

        #[test]
        fn sub_frame_range_is_dropped() {
            check!(ram_pfn_ranges(&[(0x1001, 0x1ffe)]) == vec![]);
        }

        #[test]
        fn zero_based_range_starts_at_pfn_zero() {
            check!(ram_pfn_ranges(&[(0, 0xfff)]) == vec![r(0, 1)]);
        }

        #[test]
        fn multiple_ranges_convert_in_order() {
            let ranges = [(0x1000, 0x1fff), (0x0010_0000, 0x003f_ffff)];
            check!(ram_pfn_ranges(&ranges) == vec![r(1, 1), r(0x100, 0x300)]);
        }

        #[test]
        fn non_root_zeroed_ranges_produce_nothing() {
            // Without root every /proc/iomem address reads as zero.
            check!(ram_pfn_ranges(&[(0, 0), (0, 0)]) == vec![]);
        }
    }

    mod classify_gap_ranges {
        use assert2::check;

        use super::*;

        /// Reader that serves `flags[pfn - base]` for any requested range.
        fn table_reader(base: Pfn, flags: Vec<u64>) -> impl FnMut(PfnRange, &mut [u64]) -> bool {
            move |range, out| {
                for (i, slot) in out.iter_mut().enumerate() {
                    *slot = flags[(range.start - base) as usize + i];
                }
                true
            }
        }

        #[test]
        fn empty_gaps_yield_empty_report() {
            let report = classify_gaps(&[], &mut |_, _| unreachable!());
            check!(report == GapReport::default());
        }

        #[test]
        fn one_frame_per_class() {
            let mut read = table_reader(
                Pfn::new(0),
                vec![
                    KPageFlags::BUDDY.bits(),
                    KPageFlags::LRU.bits(),
                    KPageFlags::ANON.bits(),
                    KPageFlags::SLAB.bits(),
                ],
            );
            let report = classify_gaps(&[r(0, 4)], &mut read);
            check!(report.free_bytes == PAGE_BYTES);
            check!(report.reclaimable_bytes == PAGE_BYTES);
            check!(report.in_use_bytes == PAGE_BYTES);
            check!(report.unreachable_bytes == PAGE_BYTES);
            check!(report.unknown_bytes == 0);
        }

        #[test]
        fn disjoint_gaps_accumulate() {
            let buddy = KPageFlags::BUDDY.bits();
            let mut read = table_reader(Pfn::new(0), vec![buddy, 0, buddy, 0, buddy]);
            let report = classify_gaps(&[r(0, 1), r(2, 1), r(4, 1)], &mut read);
            check!(report.free_bytes == 3 * PAGE_BYTES);
            check!(report.total_bytes() == 3 * PAGE_BYTES);
        }

        #[test]
        fn unreadable_batch_counts_as_unknown() {
            let report = classify_gaps(&[r(0, 5)], &mut |_, _| false);
            check!(report.unknown_bytes == 5 * PAGE_BYTES);
            check!(report.total_bytes() == 5 * PAGE_BYTES);
        }

        #[test]
        fn large_gap_is_read_in_batches() {
            let total = READ_BATCH_FRAMES + 10;
            let mut calls = Vec::new();
            let report = classify_gaps(&[r(0, total)], &mut |range, out| {
                calls.push(range);
                out.fill(KPageFlags::BUDDY.bits());
                true
            });
            check!(report.free_bytes == total * PAGE_BYTES);
            check!(calls == vec![r(0, READ_BATCH_FRAMES), r(READ_BATCH_FRAMES, 10)]);
        }
    }
}
