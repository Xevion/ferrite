//! Page frame numbers and the range algebra over them.
//!
//! A [`Pfn`] is a page-granular frame index (`physical address >> 12`), the
//! unit every PFN-indexed kernel interface speaks. It is deliberately a
//! separate type from [`super::phys::PhysAddr`] (a byte-granular physical
//! address) to prevent unit confusion -- a PFN is not a physical address, and
//! the two never mix without an explicit [`Pfn::to_addr`] / [`Pfn::from_addr`]
//! conversion. Zero is a valid frame.
//!
//! [`PfnRange`] and its algebra (compaction, union, subtraction, membership)
//! are pure set operations over sorted, disjoint frame ranges; coverage
//! persistence ([`super::coverage`]), gap classification ([`super::gap`]), and
//! frame-hostage culling ([`super::sieve`]) all build on them.

use std::cmp::Ordering;
use std::fmt;
use std::ops::{Add, Sub};

use serde::{Deserialize, Serialize};

use super::PAGE_BYTES;
use super::phys::PhysAddr;

/// A page frame number: `physical address >> 12`, the page-granular index used
/// by `/proc/self/pagemap` and `/proc/kpageflags`. Zero is a valid frame.
///
/// Serializes transparently as a bare `u64`, so [`PfnRange`] persists to the
/// coverage file byte-for-byte identically to a raw frame number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Pfn(pub(crate) u64);

impl Pfn {
    /// Wrap a raw frame number.
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw frame number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The byte-granular physical address of this frame's first byte.
    #[must_use]
    pub const fn to_addr(self) -> PhysAddr {
        PhysAddr(self.0 * PAGE_BYTES)
    }

    /// The frame containing a byte-granular physical address (floor division).
    #[must_use]
    pub const fn from_addr(addr: PhysAddr) -> Self {
        Self(addr.0 / PAGE_BYTES)
    }

    /// Byte offset of this frame's entry in `/proc/kpageflags` (8 bytes/frame).
    #[must_use]
    pub const fn kpageflags_offset(self) -> u64 {
        self.0 * 8
    }
}

impl fmt::Display for Pfn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Add<u64> for Pfn {
    type Output = Self;
    fn add(self, frames: u64) -> Self {
        Self(self.0 + frames)
    }
}

impl Sub for Pfn {
    type Output = u64;
    /// Frame distance between two PFNs.
    fn sub(self, other: Self) -> u64 {
        self.0 - other.0
    }
}

/// A contiguous run of physical page frames: `[start, start + count)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PfnRange {
    pub start: Pfn,
    pub count: u64,
}

impl PfnRange {
    /// The exclusive end frame (`start + count`).
    #[must_use]
    pub const fn end(self) -> Pfn {
        Pfn(self.start.0 + self.count)
    }
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
            Some(last) if last.end() == Pfn(pfn) => last.count += 1,
            _ => ranges.push(PfnRange {
                start: Pfn(pfn),
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
            Some(last) if range.start <= last.end() => {
                let end = range.end().max(last.end());
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
        let end = u.end();
        while start < end {
            // Discard covered ranges that end at or before the cursor.
            while cov.peek().is_some_and(|c| c.end() <= start) {
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
                    start = c.end();
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
pub fn contains_pfn(ranges: &[PfnRange], pfn: Pfn) -> bool {
    ranges
        .binary_search_by(|r| {
            if pfn < r.start {
                Ordering::Greater
            } else if pfn >= r.end() {
                Ordering::Less
            } else {
                Ordering::Equal
            }
        })
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(start: u64, count: u64) -> PfnRange {
        PfnRange {
            start: Pfn(start),
            count,
        }
    }

    mod pfn_type {
        use assert2::check;

        use super::*;

        #[test]
        fn addr_roundtrip() {
            check!(Pfn(5).to_addr() == PhysAddr(5 * 4096));
            check!(Pfn::from_addr(PhysAddr(5 * 4096 + 0x123)) == Pfn(5));
        }

        #[test]
        fn zero_is_a_valid_frame() {
            check!(Pfn(0).to_addr() == PhysAddr(0));
            check!(Pfn::from_addr(PhysAddr(0)) == Pfn(0));
        }

        #[test]
        fn kpageflags_offset_is_eight_bytes_per_frame() {
            check!(Pfn(0).kpageflags_offset() == 0);
            check!(Pfn(3).kpageflags_offset() == 24);
        }

        #[test]
        fn arithmetic_helpers() {
            check!(Pfn(5) + 3 == Pfn(8));
            check!(Pfn(8) - Pfn(5) == 3);
            check!(r(5, 3).end() == Pfn(8));
        }

        #[test]
        fn serializes_transparently_as_bare_u64() {
            check!(serde_json::to_string(&Pfn(42)).unwrap() == "42");
            check!(serde_json::from_str::<Pfn>("42").unwrap() == Pfn(42));
        }

        #[test]
        fn range_serializes_with_bare_u64_start() {
            let json = serde_json::to_value(r(5, 3)).unwrap();
            check!(json["start"] == 5);
            check!(json["count"] == 3);
        }
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
            check!(contains_pfn(&ranges, Pfn(5)));
            check!(contains_pfn(&ranges, Pfn(7)));
            check!(contains_pfn(&ranges, Pfn(20)));
            check!(contains_pfn(&ranges, Pfn(21)));
        }

        #[test]
        fn misses_boundaries_and_gaps() {
            let ranges = [r(5, 3), r(20, 2)];
            check!(!contains_pfn(&ranges, Pfn(4)));
            check!(!contains_pfn(&ranges, Pfn(8)));
            check!(!contains_pfn(&ranges, Pfn(19)));
            check!(!contains_pfn(&ranges, Pfn(22)));
        }

        #[test]
        fn empty_ranges_contain_nothing() {
            check!(!contains_pfn(&[], Pfn(0)));
        }
    }

    /// Property-based tests over the range algebra, using a bounded PFN
    /// universe (`0..1000`) so a [`BTreeSet<u64>`] reference model stays
    /// cheap to compute per test case.
    mod properties {
        use std::collections::BTreeSet;

        use proptest::prelude::*;

        use super::*;

        const UNIVERSE: std::ops::Range<u64> = 0..1000;

        fn dedup_nonzero(raw: &[u64]) -> BTreeSet<u64> {
            raw.iter().copied().filter(|&p| p != 0).collect()
        }

        proptest! {
            #[test]
            fn compact_pfns_ranges_sorted_disjoint_nonadjacent(
                raw in prop::collection::vec(UNIVERSE, 0..200)
            ) {
                let ranges = compact_pfns(&raw);
                for w in ranges.windows(2) {
                    // A gap of zero would have been merged by `compact_pfns`,
                    // so a real bug would show up as `<=` here.
                    prop_assert!(w[0].end() < w[1].start);
                }
            }

            #[test]
            fn compact_pfns_membership_matches_input_exactly(
                raw in prop::collection::vec(UNIVERSE, 0..200)
            ) {
                let ranges = compact_pfns(&raw);
                let expected = dedup_nonzero(&raw);

                let flattened: BTreeSet<u64> = ranges
                    .iter()
                    .flat_map(|r| r.start.get()..r.end().get())
                    .collect();
                prop_assert_eq!(&flattened, &expected);

                for &p in &expected {
                    prop_assert!(contains_pfn(&ranges, Pfn(p)));
                }
            }

            #[test]
            fn total_frames_matches_dedup_count(
                raw in prop::collection::vec(UNIVERSE, 0..200)
            ) {
                let ranges = compact_pfns(&raw);
                let expected = dedup_nonzero(&raw);
                prop_assert_eq!(total_frames(&ranges), expected.len() as u64);
            }

            #[test]
            fn merge_ranges_is_idempotent(
                raw in prop::collection::vec(UNIVERSE, 0..200)
            ) {
                let ranges = compact_pfns(&raw);
                let (merged, new) = merge_ranges(&ranges, &ranges);
                prop_assert_eq!(&merged, &ranges);
                prop_assert_eq!(new, 0);
            }

            #[test]
            fn merge_ranges_contains_every_input_frame(
                base_raw in prop::collection::vec(UNIVERSE, 0..100),
                add_raw in prop::collection::vec(UNIVERSE, 0..100),
            ) {
                let base = compact_pfns(&base_raw);
                let add = compact_pfns(&add_raw);
                let (merged, _) = merge_ranges(&base, &add);

                let expected: BTreeSet<u64> = base_raw
                    .iter()
                    .chain(add_raw.iter())
                    .copied()
                    .filter(|&p| p != 0)
                    .collect();
                for &p in &expected {
                    prop_assert!(contains_pfn(&merged, Pfn(p)));
                }
                // No extras: the merged frame count matches the deduplicated union.
                prop_assert_eq!(total_frames(&merged), expected.len() as u64);
            }

            #[test]
            fn subtract_ranges_matches_set_difference(
                a_raw in prop::collection::vec(UNIVERSE, 0..100),
                b_raw in prop::collection::vec(UNIVERSE, 0..100),
            ) {
                let a = compact_pfns(&a_raw);
                let b = compact_pfns(&b_raw);
                let result = subtract_ranges(&a, &b);

                let a_set = dedup_nonzero(&a_raw);
                let b_set = dedup_nonzero(&b_raw);
                let expected: BTreeSet<u64> = a_set.difference(&b_set).copied().collect();

                for p in UNIVERSE {
                    prop_assert_eq!(contains_pfn(&result, Pfn(p)), expected.contains(&p));
                }
            }
        }
    }
}
