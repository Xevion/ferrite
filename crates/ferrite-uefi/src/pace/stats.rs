//! What each finished test of the pass measured: its rate over work time alone, how that compares
//! with what this machine did before, and how steady it was.

use alloc::vec::Vec;
use core::fmt;

use ferrite_mem::eta::BasisPoints;
use ferrite_mem::histogram::Histogram;

/// A rate is compared with the saved one only when it differs by more than this share of it. A
/// memory-bound test repeats on one machine within a percent or two, so a bigger gap is a change
/// worth a column; it is a starting value to tune on the machines this runs on.
const DELTA_SHOWN_ABOVE_PERCENT: u64 = 3;
/// A test is unsteady when half the span from its 10th to its 90th percentile rate is more than
/// this share of its median (1000 is 10%): a steady test reads within a few percent second to second.
const UNSTEADY_ABOVE: u16 = 1000;
/// Fewer one-second samples than this put the 10th and 90th percentiles at the extremes.
const MIN_SAMPLES: u64 = 5;

/// How a rate compares with the one saved for this machine, in whole percent: positive is faster.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Delta(i16);

impl Delta {
    /// `rate` against `saved`, when it is far enough from it to be worth showing.
    #[must_use]
    pub fn between(rate: u64, saved: u64) -> Option<Self> {
        if saved == 0
            || u128::from(rate.abs_diff(saved)) * 100 <= u128::from(saved) * u128::from(DELTA_SHOWN_ABOVE_PERCENT)
        {
            return None;
        }
        let (rate, saved) = (i128::from(rate), i128::from(saved));
        let diff = rate - saved;
        let percent = (diff * 200 + diff.signum() * saved) / (2 * saved);
        Some(Self(i16::try_from(percent).unwrap_or(if percent < 0 { i16::MIN } else { i16::MAX })))
    }

    #[must_use]
    pub const fn percent(self) -> i16 {
        self.0
    }
}

/// `+4%` or `−4%`, with the real minus sign.
impl fmt::Display for Delta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}%", if self.0 < 0 { '\u{2212}' } else { '+' }, self.0.unsigned_abs())
    }
}

/// The spread of a test's per-second rates around their median. The median, not the mean, because a
/// brief stall (a firmware interrupt, the Wi-Fi poll) drags a mean and leaves the median alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Steadiness {
    /// Bytes per second.
    pub median: u64,
    /// Half the span from the 10th to the 90th percentile, as a share of the median.
    pub spread: BasisPoints,
}

impl Steadiness {
    /// The steadiness of the rates in `rates` (bytes per second), if there are enough.
    #[must_use]
    pub fn of(rates: &Histogram) -> Option<Self> {
        if rates.count() < MIN_SAMPLES {
            return None;
        }
        let at = |raw| BasisPoints::new(raw).and_then(|q| rates.quantile(q));
        let (low, median, high) = (at(1000)?, at(5000)?, at(9000)?);
        if median == 0 {
            return None;
        }
        let spread = u128::from(high.saturating_sub(low)) * u128::from(BasisPoints::WHOLE) / 2 / u128::from(median);
        Some(Self { median, spread: BasisPoints::clamped(u64::try_from(spread).unwrap_or(u64::MAX)) })
    }

    #[must_use]
    pub const fn unsteady(self) -> bool {
        self.spread.get() > UNSTEADY_ABOVE
    }
}

/// One finished test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TestStat {
    pub test: &'static str,
    pub pass: u64,
    /// Bytes of work per second over the test's work time, its waits left out.
    pub rate: Option<u64>,
    /// How the rate compares with the time saved for this machine, if by enough to show.
    pub delta: Option<Delta>,
    pub steadiness: Option<Steadiness>,
}

/// The finished tests of the pass, by the test's place in it; `revision` moves whenever one is set.
#[derive(Clone, Debug, Default)]
pub struct TestStats {
    pub revision: u64,
    tests: Vec<Option<TestStat>>,
}

impl TestStats {
    /// Forget every test, with room for `tests` of them.
    pub fn reset(&mut self, tests: usize) {
        self.tests.clear();
        self.tests.resize(tests, None);
        self.revision += 1;
    }

    pub fn set(&mut self, index: usize, stat: TestStat) {
        if let Some(slot) = self.tests.get_mut(index) {
            *slot = Some(stat);
            self.revision += 1;
        }
    }

    /// The latest finished test called `test`, if it was in pass `pass`.
    #[must_use]
    pub fn of(&self, test: &str, pass: u64) -> Option<&TestStat> {
        self.tests.iter().flatten().find(|stat| stat.test == test && stat.pass == pass)
    }

    /// The test at `index`, if it is `test` and has finished.
    #[must_use]
    pub fn get(&self, index: usize, test: &str) -> Option<&TestStat> {
        self.tests.get(index)?.as_ref().filter(|stat| stat.test == test)
    }
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::same(1000, 1000, None)]
    #[case::inside_the_threshold_below(970, 1000, None)]
    #[case::inside_the_threshold_above(1030, 1000, None)]
    #[case::just_past_it(969, 1000, Some(-3))]
    #[case::faster(1040, 1000, Some(4))]
    #[case::rounds_to_nearest(1046, 1000, Some(5))]
    #[case::slower_rounds_to_nearest(954, 1000, Some(-5))]
    #[case::half_as_fast(500, 1000, Some(-50))]
    #[case::twice_as_fast(2000, 1000, Some(100))]
    #[case::nothing_saved(500, 0, None)]
    fn a_rate_is_set_against_the_saved_one_only_beyond_the_threshold(
        #[case] rate: u64,
        #[case] saved: u64,
        #[case] percent: Option<i16>,
    ) {
        check!(Delta::between(rate, saved).map(Delta::percent) == percent);
    }

    #[test]
    fn a_delta_prints_its_sign() {
        check!(std::format!("{}", Delta::between(1040, 1000).unwrap()) == "+4%");
        check!(std::format!("{}", Delta::between(900, 1000).unwrap()) == "\u{2212}10%");
    }

    fn rates(samples: impl IntoIterator<Item = u64>) -> Histogram {
        let mut hist = Histogram::new();
        samples.into_iter().for_each(|s| hist.record(s));
        hist
    }

    #[test]
    fn a_steady_test_has_a_small_spread_and_a_stall_does_not_move_its_median() {
        // 12 GB/s give or take 2%, with two stalls of a second each.
        let mut samples: Vec<u64> = (0..40).map(|i| 11_760_000_000 + (i % 5) * 120_000_000).collect();
        samples.extend([900_000_000, 1_500_000_000]);
        assert!(let Some(steady) = Steadiness::of(&rates(samples)));
        check!(steady.median.abs_diff(12_000_000_000) < 200_000_000, "{}", steady.median);
        check!(steady.spread.get() < 400, "{}", steady.spread);
        check!(!steady.unsteady());
    }

    #[test]
    fn a_test_that_swings_between_two_speeds_is_unsteady() {
        let samples = (0..30).map(|i| if i % 2 == 0 { 8_000_000_000 } else { 12_000_000_000 });
        assert!(let Some(swinging) = Steadiness::of(&rates(samples)));
        check!(swinging.spread.get() > 1000, "{}", swinging.spread);
        check!(swinging.unsteady());
    }

    #[test]
    fn too_few_seconds_say_nothing_about_steadiness() {
        check!(Steadiness::of(&rates([5_000_000_000; 4])).is_none());
        check!(Steadiness::of(&rates([5_000_000_000; 5])).is_some());
        check!(Steadiness::of(&Histogram::new()).is_none());
    }

    #[test]
    fn a_stat_is_only_found_for_its_own_test() {
        let stat = TestStat { test: "block move", pass: 2, rate: Some(1), delta: None, steadiness: None };
        let mut stats = TestStats::default();
        stats.reset(3);
        stats.set(1, stat);
        check!(stats.get(1, "block move") == Some(&stat));
        check!(stats.get(1, "bit fade").is_none() && stats.get(0, "block move").is_none());
        stats.set(9, stat);
        check!(stats.get(9, "block move").is_none(), "a test that is not in the pass is not kept");
    }
}
