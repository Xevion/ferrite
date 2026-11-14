//! A fixed-size histogram of `u64` samples with log-linear buckets, for quantiles of rates and times.
//!
//! The layout is `HdrHistogram`'s: values below [`SUB_BUCKETS`] each have a bucket, and every power
//! of two above that is split into [`SUB_BUCKETS`] equal buckets, so a bucket is never wider than
//! 1/32 of the values in it. A quantile is reported as the middle of its bucket, which is at most
//! 1/64 (1.6%) from the sample that holds that rank. Recording a sample touches one counter and
//! allocates nothing; the counters are one allocation made by [`Histogram::new`], [`Histogram::BYTES`]
//! long (4.5 KiB), and two histograms [`merge`](Histogram::merge) by adding their counters.

use alloc::boxed::Box;
use alloc::vec;

use crate::eta::BasisPoints;

/// Bits that pick a bucket within a power of two.
const SUB_BITS: u32 = 5;
/// Buckets in each power of two, and the values that have a bucket of their own.
pub const SUB_BUCKETS: usize = 1 << SUB_BITS;
/// Values from this one on are recorded as this one: 2^40, a terabyte per second or 34 years of ms.
const LIMIT_BITS: u32 = 40;
/// One group of [`SUB_BUCKETS`] for each power of two from `2^SUB_BITS` to the limit, and the first.
const BUCKETS: usize = (LIMIT_BITS - SUB_BITS + 1) as usize * SUB_BUCKETS;

/// A histogram of samples, `0` to [`Histogram::MAX`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Histogram {
    counts: Box<[u32]>,
    total: u64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

/// The bucket that holds `value`.
fn index(value: u64) -> usize {
    let value = value.min(Histogram::MAX);
    if value < SUB_BUCKETS as u64 {
        return value as usize;
    }
    let shift = value.ilog2() - SUB_BITS;
    ((shift as usize + 1) << SUB_BITS) + (value >> shift) as usize - SUB_BUCKETS
}

/// The values bucket `index` holds: the first and how many.
fn span(index: usize) -> (u64, u64) {
    let (group, sub) = (index >> SUB_BITS, index & (SUB_BUCKETS - 1));
    if group == 0 {
        return (sub as u64, 1);
    }
    let shift = group as u32 - 1;
    (((SUB_BUCKETS + sub) as u64) << shift, 1 << shift)
}

impl Histogram {
    /// The largest sample told apart; larger ones are counted as this.
    pub const MAX: u64 = (1 << LIMIT_BITS) - 1;
    /// Memory the counters take.
    pub const BYTES: usize = BUCKETS * size_of::<u32>();

    #[must_use]
    pub fn new() -> Self {
        Self { counts: vec![0; BUCKETS].into_boxed_slice(), total: 0 }
    }

    /// Count one sample.
    pub fn record(&mut self, value: u64) {
        if let Some(count) = self.counts[index(value)].checked_add(1) {
            self.counts[index(value)] = count;
            self.total += 1;
        }
    }

    /// How many samples there are.
    #[must_use]
    pub const fn count(&self) -> u64 {
        self.total
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// Forget every sample.
    pub fn clear(&mut self) {
        self.counts.fill(0);
        self.total = 0;
    }

    /// Add the samples of `other` to this one.
    pub fn merge(&mut self, other: &Self) {
        for (mine, theirs) in self.counts.iter_mut().zip(&other.counts) {
            *mine = mine.saturating_add(*theirs);
        }
        self.total = self.counts.iter().map(|&c| u64::from(c)).sum();
    }

    /// The sample at quantile `q` (the middle of its bucket): the smallest recorded value that at
    /// least a fraction `q` of the samples do not exceed, by nearest rank. `None` when empty.
    #[must_use]
    pub fn quantile(&self, q: BasisPoints) -> Option<u64> {
        if self.total == 0 {
            return None;
        }
        let whole = u128::from(BasisPoints::WHOLE);
        let rank = (u128::from(self.total) * u128::from(q.get())).div_ceil(whole).max(1);
        let mut seen = 0_u128;
        for (i, &count) in self.counts.iter().enumerate() {
            seen += u128::from(count);
            if seen >= rank {
                let (first, width) = span(i);
                return Some(first + width / 2);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use assert2::{assert, check};
    use rand_chacha::ChaCha8Rng;
    use rand_core::{Rng, SeedableRng};
    use rstest::rstest;

    use super::*;

    fn q(raw: u16) -> BasisPoints {
        BasisPoints::new(raw).unwrap()
    }

    /// The sample at rank `ceil(q * n)` of the sorted samples.
    fn nearest_rank(sorted: &[u64], q: BasisPoints) -> u64 {
        let rank = (sorted.len() * usize::from(q.get())).div_ceil(10_000).max(1);
        sorted[rank - 1]
    }

    /// Samples spread over every magnitude up to 2^38.
    fn wide(rng: &mut ChaCha8Rng, n: usize) -> Vec<u64> {
        (0..n)
            .map(|_| {
                let magnitude = rng.next_u32() % 38;
                (1_u64 << magnitude) | (rng.next_u64() & ((1_u64 << magnitude) - 1))
            })
            .collect()
    }

    /// Samples within a few percent of 12 GB/s, with the odd stall: what a steady test looks like.
    fn steady(rng: &mut ChaCha8Rng, n: usize) -> Vec<u64> {
        (0..n)
            .map(|_| {
                let jitter = rng.next_u64() % 480_000_000;
                if rng.next_u32().is_multiple_of(50) { jitter } else { 11_760_000_000 + jitter }
            })
            .collect()
    }

    #[rstest]
    #[case::every_magnitude(wide)]
    #[case::a_steady_test(steady)]
    fn quantiles_are_the_nearest_rank_to_within_the_bucket(#[case] make: fn(&mut ChaCha8Rng, usize) -> Vec<u64>) {
        let mut rng = ChaCha8Rng::seed_from_u64(0x00F0_0D5E);
        let mut samples = make(&mut rng, 20_000);
        let mut hist = Histogram::new();
        for &s in &samples {
            hist.record(s);
        }
        samples.sort_unstable();
        check!(hist.count() == samples.len() as u64);
        for raw in [0, 1, 100, 1000, 2500, 5000, 7500, 9000, 9900, 9999, 10_000] {
            let want = nearest_rank(&samples, q(raw));
            assert!(let Some(got) = hist.quantile(q(raw)));
            let (first, width) = span(index(want));
            check!((first..first + width).contains(&got), "q {raw}: {got} is not in the bucket of {want}");
            check!(got.abs_diff(want) * 64 <= want, "q {raw}: {got} against {want}");
        }
    }

    #[test]
    fn merging_is_recording_everything_in_one() {
        let mut rng = ChaCha8Rng::seed_from_u64(7);
        let (a, b) = (wide(&mut rng, 3000), steady(&mut rng, 2000));
        let (mut left, mut right, mut both) = (Histogram::new(), Histogram::new(), Histogram::new());
        for &s in &a {
            left.record(s);
            both.record(s);
        }
        for &s in &b {
            right.record(s);
            both.record(s);
        }
        left.merge(&right);
        check!(left == both);
        check!(left.count() == 5000);
    }

    #[test]
    fn buckets_tile_the_range_and_are_never_wider_than_a_32nd() {
        let mut next = 0;
        for i in 0..BUCKETS {
            let (first, width) = span(i);
            check!(first == next, "bucket {i} starts at {first}, the one before ended at {next}");
            check!(index(first) == i && index(first + width - 1) == i);
            if first >= SUB_BUCKETS as u64 {
                check!(width * 32 <= first, "bucket {i}: {width} wide at {first}");
            } else {
                check!(width == 1);
            }
            next = first + width;
        }
        check!(next == Histogram::MAX + 1);
    }

    #[rstest]
    #[case::zero(0, 0)]
    #[case::last_exact_value(31, 31)]
    #[case::first_wide_octave(32, 32)]
    #[case::octave_end(63, 63)]
    #[case::pairs_start(64, 64)]
    #[case::pair_shares_a_bucket(65, 64)]
    #[case::the_limit(Histogram::MAX, Histogram::MAX - (1 << (LIMIT_BITS - SUB_BITS - 1)) + 1)]
    #[case::beyond_the_limit(u64::MAX, Histogram::MAX - (1 << (LIMIT_BITS - SUB_BITS - 1)) + 1)]
    fn a_value_lands_in_the_bucket_that_starts_where_it_should(#[case] value: u64, #[case] first: u64) {
        check!(span(index(value)).0 == first);
    }

    #[test]
    fn empty_and_single_sample_histograms() {
        let mut hist = Histogram::new();
        check!(hist.quantile(q(5000)).is_none() && hist.is_empty());
        hist.record(1234);
        check!(hist.quantile(q(0)) == hist.quantile(q(10_000)));
        let got = hist.quantile(q(5000)).unwrap();
        check!(got.abs_diff(1234) * 64 <= 1234);
        hist.clear();
        check!(hist.is_empty() && hist.quantile(q(5000)).is_none());
    }
}
