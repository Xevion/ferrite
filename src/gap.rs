//! Gap classification: explaining the untested remainder of installed RAM.
//!
//! "Coverage: 81%" hides whether the other 19% is reachable at all. This
//! module scans `/proc/kpageflags` (indexed by PFN, root-readable) for every
//! System RAM frame outside the covered set and buckets it by what userspace
//! could do about it: acquirable right now, reclaimable under pressure, in
//! use by other processes, or unreachable by design (kernel text, slab,
//! reserved). See `docs/COVERAGE.md` ("Honest denominators") and XEV-1019.

use serde::Serialize;

use crate::coverage::{FRAME_BYTES, PfnRange};

// Public /proc/kpageflags bits (Documentation/admin-guide/mm/pagemap.rst).
const KPF_LRU: u64 = 1 << 5;
const KPF_SLAB: u64 = 1 << 7;
const KPF_BUDDY: u64 = 1 << 10;
const KPF_ANON: u64 = 1 << 12;
const KPF_SWAPBACKED: u64 = 1 << 14;
const KPF_HWPOISON: u64 = 1 << 19;
const KPF_NOPAGE: u64 = 1 << 20;
const KPF_OFFLINE: u64 = 1 << 23;
const KPF_PGTABLE: u64 = 1 << 26;

/// Frames per batched `/proc/kpageflags` read (512 KiB of flag data).
const READ_BATCH_FRAMES: u64 = 65536;

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

/// Classify one frame's raw kpageflags word.
#[must_use]
pub fn classify(flags: u64) -> FrameClass {
    if flags & (KPF_NOPAGE | KPF_OFFLINE | KPF_HWPOISON | KPF_SLAB | KPF_PGTABLE) != 0 {
        FrameClass::Unreachable
    } else if flags & KPF_BUDDY != 0 {
        FrameClass::Free
    } else if flags & (KPF_ANON | KPF_SWAPBACKED) != 0 {
        FrameClass::InUse
    } else if flags & KPF_LRU != 0 {
        FrameClass::Reclaimable
    } else {
        // Kernel text/data, reserved, driver pages: no public flags.
        FrameClass::Unreachable
    }
}

/// Byte totals per [`FrameClass`] over the untested remainder.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct GapReport {
    pub free_bytes: u64,
    pub reclaimable_bytes: u64,
    pub in_use_bytes: u64,
    pub unreachable_bytes: u64,
    /// Frames whose flags could not be read.
    #[serde(skip_serializing_if = "is_zero")]
    pub unknown_bytes: u64,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(v: &u64) -> bool {
    *v == 0
}

impl GapReport {
    /// Total untested bytes across all classes.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.free_bytes
            + self.reclaimable_bytes
            + self.in_use_bytes
            + self.unreachable_bytes
            + self.unknown_bytes
    }

    fn add(&mut self, class: FrameClass) {
        let bucket = match class {
            FrameClass::Free => &mut self.free_bytes,
            FrameClass::Reclaimable => &mut self.reclaimable_bytes,
            FrameClass::InUse => &mut self.in_use_bytes,
            FrameClass::Unreachable => &mut self.unreachable_bytes,
        };
        *bucket += FRAME_BYTES;
    }
}

/// Convert inclusive System RAM byte ranges (as parsed from `/proc/iomem`)
/// into the PFN ranges they fully contain.
#[must_use]
pub fn ram_pfn_ranges(ranges: &[(u64, u64)]) -> Vec<PfnRange> {
    ranges
        .iter()
        .filter_map(|&(start, end)| {
            let first = start.div_ceil(FRAME_BYTES);
            let last = (end + 1) / FRAME_BYTES;
            (last > first).then(|| PfnRange {
                start: first,
                count: last - first,
            })
        })
        .collect()
}

/// Classify every frame in `gaps`, reading raw kpageflags in batches via
/// `read`. `read(range, out)` must fill `out` (length `range.count`) with the
/// flags for `range` and return `true`; `false` counts the batch as unknown.
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
            #[allow(clippy::cast_possible_truncation)]
            let out = &mut buf[..count as usize];
            if read(range, out) {
                for &flags in &*out {
                    report.add(classify(flags));
                }
            } else {
                report.unknown_bytes += count * FRAME_BYTES;
            }
            offset += count;
        }
    }
    report
}

/// Scan the live system: classify every System RAM frame outside `covered`
/// (sorted/disjoint PFN ranges). Returns `None` when `/proc/iomem` reads as
/// zeroed (non-root) or `/proc/kpageflags` cannot be opened.
#[cfg_attr(coverage_nightly, coverage(off))]
#[must_use]
pub fn classify_system_gaps(covered: &[PfnRange]) -> Option<GapReport> {
    let iomem = std::fs::read_to_string("/proc/iomem").ok()?;
    let universe = ram_pfn_ranges(&crate::sysmem::system_ram_ranges(&iomem));
    if universe.is_empty() {
        return None;
    }
    let file = std::fs::File::open("/proc/kpageflags").ok()?;
    let gaps = crate::coverage::subtract_ranges(&universe, covered);

    #[allow(clippy::cast_possible_truncation)]
    let mut bytes = vec![0u8; READ_BATCH_FRAMES as usize * 8];
    Some(classify_gaps(&gaps, &mut |range, out| {
        let Ok(len) = usize::try_from(range.count * 8) else {
            return false;
        };
        let Ok(offset) = i64::try_from(range.start * 8) else {
            return false;
        };
        if crate::phys::pread_exact(&file, &mut bytes[..len], offset).is_err() {
            return false;
        }
        for (slot, chunk) in out.iter_mut().zip(bytes[..len].chunks_exact(8)) {
            // chunks_exact(8) guarantees the conversion; 0 is unreachable.
            *slot = chunk.try_into().map_or(0, u64::from_le_bytes);
        }
        true
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(start: u64, count: u64) -> PfnRange {
        PfnRange { start, count }
    }

    mod classify_flags {
        use assert2::check;

        use super::*;

        #[test]
        fn buddy_is_free() {
            check!(classify(KPF_BUDDY) == FrameClass::Free);
        }

        #[test]
        fn file_cache_is_reclaimable() {
            // Typical page-cache frame: LRU (+ uptodate/referenced noise).
            check!(classify(KPF_LRU) == FrameClass::Reclaimable);
            check!(classify(KPF_LRU | (1 << 3) | (1 << 2)) == FrameClass::Reclaimable);
        }

        #[test]
        fn anon_is_in_use() {
            check!(classify(KPF_ANON | KPF_LRU | KPF_SWAPBACKED) == FrameClass::InUse);
        }

        #[test]
        fn shmem_without_anon_is_in_use() {
            // tmpfs/shmem: swap-backed but not anonymous.
            check!(classify(KPF_SWAPBACKED | KPF_LRU) == FrameClass::InUse);
        }

        #[test]
        fn slab_and_pgtable_are_unreachable() {
            check!(classify(KPF_SLAB) == FrameClass::Unreachable);
            check!(classify(KPF_PGTABLE) == FrameClass::Unreachable);
        }

        #[test]
        fn nopage_offline_hwpoison_are_unreachable() {
            check!(classify(KPF_NOPAGE) == FrameClass::Unreachable);
            check!(classify(KPF_OFFLINE) == FrameClass::Unreachable);
            check!(classify(KPF_HWPOISON) == FrameClass::Unreachable);
        }

        #[test]
        fn hwpoison_trumps_everything() {
            check!(classify(KPF_HWPOISON | KPF_BUDDY) == FrameClass::Unreachable);
            check!(classify(KPF_HWPOISON | KPF_ANON | KPF_LRU) == FrameClass::Unreachable);
        }

        #[test]
        fn no_flags_is_unreachable() {
            // Kernel text/data and reserved frames expose no public flags.
            check!(classify(0) == FrameClass::Unreachable);
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
        fn table_reader(base: u64, flags: Vec<u64>) -> impl FnMut(PfnRange, &mut [u64]) -> bool {
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
            let mut read = table_reader(0, vec![KPF_BUDDY, KPF_LRU, KPF_ANON, KPF_SLAB]);
            let report = classify_gaps(&[r(0, 4)], &mut read);
            check!(report.free_bytes == FRAME_BYTES);
            check!(report.reclaimable_bytes == FRAME_BYTES);
            check!(report.in_use_bytes == FRAME_BYTES);
            check!(report.unreachable_bytes == FRAME_BYTES);
            check!(report.unknown_bytes == 0);
        }

        #[test]
        fn disjoint_gaps_accumulate() {
            let mut read = table_reader(0, vec![KPF_BUDDY, 0, KPF_BUDDY, 0, KPF_BUDDY]);
            let report = classify_gaps(&[r(0, 1), r(2, 1), r(4, 1)], &mut read);
            check!(report.free_bytes == 3 * FRAME_BYTES);
            check!(report.total_bytes() == 3 * FRAME_BYTES);
        }

        #[test]
        fn unreadable_batch_counts_as_unknown() {
            let report = classify_gaps(&[r(0, 5)], &mut |_, _| false);
            check!(report.unknown_bytes == 5 * FRAME_BYTES);
            check!(report.total_bytes() == 5 * FRAME_BYTES);
        }

        #[test]
        fn large_gap_is_read_in_batches() {
            let total = READ_BATCH_FRAMES + 10;
            let mut calls = Vec::new();
            let report = classify_gaps(&[r(0, total)], &mut |range, out| {
                calls.push(range);
                out.fill(KPF_BUDDY);
                true
            });
            check!(report.free_bytes == total * FRAME_BYTES);
            check!(calls == vec![r(0, READ_BATCH_FRAMES), r(READ_BATCH_FRAMES, 10)]);
        }
    }
}
