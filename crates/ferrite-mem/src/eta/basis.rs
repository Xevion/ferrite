//! A fraction of a whole in hundredths of a percent.

use core::fmt;
use core::time::Duration;

/// How far along something is, from nothing to the whole, in hundredths of a percent (0 to 10 000).
///
/// Every constructor rounds down, so `100.00%` means done and never "nearly", and the value comes
/// from the raw counts or times it measures, never from a percentage that was already rounded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BasisPoints(u16);

impl BasisPoints {
    /// The raw value of the whole.
    pub const WHOLE: u16 = 10_000;
    pub const ZERO: Self = Self(0);
    pub const FULL: Self = Self(Self::WHOLE);

    /// `raw` hundredths of a percent, if that is no more than the whole.
    #[must_use]
    pub const fn new(raw: u16) -> Option<Self> {
        if raw <= Self::WHOLE { Some(Self(raw)) } else { None }
    }

    /// `raw` hundredths of a percent, or the whole if it is more.
    #[must_use]
    pub const fn clamped(raw: u64) -> Self {
        if raw >= Self::WHOLE as u64 { Self::FULL } else { Self(raw as u16) }
    }

    /// `done` of `total`; an empty `total` is complete, and `done` beyond it counts as all of it.
    #[must_use]
    pub const fn of(done: u64, total: u64) -> Self {
        if total == 0 || done >= total {
            return Self::FULL;
        }
        Self((done as u128 * Self::WHOLE as u128 / total as u128) as u16)
    }

    /// `done` of `total` by time, with the same edges as [`BasisPoints::of`].
    #[must_use]
    pub const fn of_time(done: Duration, total: Duration) -> Self {
        let (done, total) = (done.as_nanos(), total.as_nanos());
        if total == 0 || done >= total {
            return Self::FULL;
        }
        Self((done * Self::WHOLE as u128 / total) as u16)
    }

    /// The raw value, 0 to 10 000.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }

    /// What is left: the whole less this.
    #[must_use]
    pub const fn left(self) -> Self {
        Self(Self::WHOLE - self.0)
    }

    /// This fraction of `whole`, rounded down: the filled width of a bar `whole` pixels across.
    #[must_use]
    pub const fn of_whole(self, whole: u32) -> u32 {
        (whole as u64 * self.0 as u64 / Self::WHOLE as u64) as u32
    }

    /// Whole percent, rounded down.
    #[must_use]
    pub const fn percent(self) -> u16 {
        self.0 / 100
    }
}

/// `41.37%`.
impl fmt::Display for BasisPoints {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:02}%", self.0 / 100, self.0 % 100)
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::none(0, 100, 0)]
    #[case::rounds_down(1, 3, 3333)]
    #[case::just_short(9_999, 10_000, 9999)]
    #[case::one_hundredth(1, 10_000, 1)]
    #[case::less_than_one_hundredth(1, 10_001, 0)]
    #[case::all(7, 7, 10_000)]
    #[case::past_all(9, 7, 10_000)]
    #[case::empty_whole_is_done(0, 0, 10_000)]
    #[case::huge_counts(u64::MAX - 1, u64::MAX, 9999)]
    fn a_fraction_of_counts_rounds_down_and_stays_in_range(#[case] done: u64, #[case] total: u64, #[case] raw: u16) {
        check!(BasisPoints::of(done, total).get() == raw);
    }

    #[test]
    fn time_follows_the_same_edges() {
        let s = Duration::from_secs;
        check!(BasisPoints::of_time(s(1), s(3)).get() == 3333);
        check!(BasisPoints::of_time(Duration::from_nanos(1), s(3600)).get() == 0);
        check!(BasisPoints::of_time(s(4), s(3)) == BasisPoints::FULL);
        check!(BasisPoints::of_time(s(0), Duration::ZERO) == BasisPoints::FULL);
    }

    #[rstest]
    #[case(0, "0.00%")]
    #[case(5, "0.05%")]
    #[case(4137, "41.37%")]
    #[case(9999, "99.99%")]
    #[case(10_000, "100.00%")]
    fn shown_to_two_decimals(#[case] raw: u16, #[case] text: &str) {
        check!(BasisPoints::new(raw).map(|b| std::format!("{b}")).as_deref() == Some(text));
    }

    #[test]
    fn only_a_fraction_of_the_whole_can_be_made() {
        check!(BasisPoints::new(10_001).is_none());
        check!(BasisPoints::clamped(10_001) == BasisPoints::FULL);
        check!(BasisPoints::clamped(77).get() == 77);
    }

    #[test]
    fn a_bar_fills_by_whole_pixels_and_never_past_its_end() {
        let bp = |raw| BasisPoints::new(raw).unwrap();
        check!(bp(0).of_whole(1000) == 0);
        check!(bp(9999).of_whole(1000) == 999);
        check!(bp(10_000).of_whole(1000) == 1000);
        check!(bp(3333).left().get() == 6667);
        check!(bp(4137).percent() == 41);
    }
}
