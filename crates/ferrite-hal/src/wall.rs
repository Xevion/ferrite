//! UTC on top of the monotonic clock: a point in UTC, and a [`WallClock`] that maps an [`Instant`] to
//! one, seeded from a coarse source (the firmware's RTC) and corrected by trusted fixes (NTP).
//!
//! The monotonic clock is what the machine measures with, and its rate is off by a fraction of a
//! percent (the TSC of the target laptop runs about 0.1% slow). Each fix steps the clock to the true
//! time, and the drift seen between fixes is applied until the next one.

use core::fmt;
use core::time::Duration;

use crate::Instant;

const NANOS_PER_SEC: i128 = 1_000_000_000;
const PPM: i128 = 1_000_000;

/// The largest rate error of the monotonic clock that is believed, in parts per million.
pub const MAX_RATE_PPM: i32 = 2000;

/// A step at a fix larger than this is not drift but a jump (a wrong seed, a changed server), so what it
/// implies about the rate is discarded.
const WILD_STEP: Duration = Duration::from_secs(5);

/// Fixes closer together than this say too little about the rate: at tens of milliseconds of noise,
/// ten minutes keeps the estimate within about 100 ppm.
const MIN_BASELINE: Duration = Duration::from_mins(10);

/// A point in UTC, as time since the Unix epoch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UnixTime(Duration);

impl UnixTime {
    #[must_use]
    pub const fn from_epoch(since_epoch: Duration) -> Self {
        Self(since_epoch)
    }

    #[must_use]
    pub const fn since_epoch(self) -> Duration {
        self.0
    }

    /// Whole milliseconds since the epoch, for a JSON number.
    #[must_use]
    pub fn as_millis(self) -> u64 {
        u64::try_from(self.0.as_millis()).unwrap_or(u64::MAX)
    }

    #[must_use]
    pub const fn checked_add(self, d: Duration) -> Option<Self> {
        match self.0.checked_add(d) {
            Some(t) => Some(Self(t)),
            None => None,
        }
    }

    /// `self` minus `d`, or the epoch itself when that would be before it.
    #[must_use]
    pub const fn saturating_sub(self, d: Duration) -> Self {
        Self(self.0.saturating_sub(d))
    }

    fn nanos(self) -> i128 {
        i128::try_from(self.0.as_nanos()).unwrap_or(i128::MAX)
    }

    /// The time `nanos` after the epoch; the epoch for a negative count.
    fn from_nanos(nanos: i128) -> Self {
        let nanos = nanos.max(0);
        let secs = u64::try_from(nanos / NANOS_PER_SEC).unwrap_or(u64::MAX);
        Self(Duration::new(secs, u32::try_from(nanos % NANOS_PER_SEC).unwrap_or(0)))
    }
}

/// How far one time is from another: positive when the second is later.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Offset(i128);

impl Offset {
    /// `to` minus `from`.
    #[must_use]
    pub fn between(from: UnixTime, to: UnixTime) -> Self {
        Self(to.nanos() - from.nanos())
    }

    /// The size of the offset without its sign.
    #[must_use]
    pub fn abs(self) -> Duration {
        let nanos = self.0.unsigned_abs();
        Duration::new(u64::try_from(nanos / 1_000_000_000).unwrap_or(u64::MAX), (nanos % 1_000_000_000) as u32)
    }

    #[must_use]
    pub const fn is_negative(self) -> bool {
        self.0 < 0
    }
}

/// `+12.345 ms` below a second, `-3.250 s` from a second on.
impl fmt::Display for Offset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let micros = self.0.unsigned_abs() / 1000;
        let sign = if self.is_negative() { '-' } else { '+' };
        if micros < 1_000_000 {
            write!(f, "{sign}{}.{:03} ms", micros / 1000, micros % 1000)
        } else {
            write!(f, "{sign}{}.{:03} s", micros / 1_000_000, micros / 1000 % 1000)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    /// Seeded from the battery-backed clock, which can be minutes or years off.
    Rtc,
    Fixed,
}

/// What a [`WallClock::fix`] changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Correction {
    /// The true time minus what the clock said just before.
    pub step: Offset,
    /// The rate error now applied between fixes, in parts per million: positive when the monotonic
    /// clock runs slow.
    pub rate_ppm: i32,
    /// This fix discarded the rate estimate, because the clock was too far off to be drifting.
    pub rate_reset: bool,
}

/// UTC at an anchor on the monotonic clock, and the rate it advances at from there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WallClock {
    anchor: Instant,
    utc: UnixTime,
    rate_ppm: i32,
    source: Source,
    /// The fix the rate is measured from, once there is one.
    base: Option<(Instant, UnixTime)>,
    rate_known: bool,
}

impl WallClock {
    /// A clock that read `utc` at `at` from a source that is not to be trusted to the second.
    #[must_use]
    pub const fn from_rtc(at: Instant, utc: UnixTime) -> Self {
        Self { anchor: at, utc, rate_ppm: 0, source: Source::Rtc, base: None, rate_known: false }
    }

    /// A clock that has never had a source but a fix: `utc` is the true time at `at`.
    #[must_use]
    pub fn from_fix(at: Instant, utc: UnixTime) -> Self {
        let mut clock = Self::from_rtc(at, utc);
        clock.fix(at, utc);
        clock
    }

    /// Whether a fix has corrected this clock.
    #[must_use]
    pub fn is_fixed(&self) -> bool {
        self.source == Source::Fixed
    }

    /// The rate error applied, in parts per million.
    #[must_use]
    pub const fn rate_ppm(&self) -> i32 {
        self.rate_ppm
    }

    /// UTC at `instant`, before the anchor or after it.
    #[must_use]
    pub fn at(&self, instant: Instant) -> UnixTime {
        let (ahead, span) =
            if instant >= self.anchor { (true, instant - self.anchor) } else { (false, self.anchor - instant) };
        let nanos = i128::try_from(span.as_nanos()).unwrap_or(i128::MAX);
        let scaled = nanos + nanos * i128::from(self.rate_ppm) / PPM;
        UnixTime::from_nanos(self.utc.nanos() + if ahead { scaled } else { -scaled })
    }

    /// Step the clock to `utc`, the true time at `at`, and learn the monotonic clock's rate error
    /// from the fixes so far.
    pub fn fix(&mut self, at: Instant, utc: UnixTime) -> Correction {
        let step = Offset::between(self.at(at), utc);
        let mut rate_reset = false;
        match self.base {
            Some(_) if step.abs() > WILD_STEP => {
                self.rate_ppm = 0;
                self.rate_known = false;
                self.base = Some((at, utc));
                rate_reset = true;
            },
            None => self.base = Some((at, utc)),
            Some((since, then)) if at - since >= MIN_BASELINE => {
                let measured = measured_rate(at - since, Offset::between(then, utc));
                self.rate_ppm = if self.rate_known { self.rate_ppm.midpoint(measured) } else { measured };
                self.rate_known = true;
                self.base = Some((at, utc));
            },
            Some(_) => {},
        }
        self.anchor = at;
        self.utc = utc;
        self.source = Source::Fixed;
        Correction { step, rate_ppm: self.rate_ppm, rate_reset }
    }
}

/// The rate error over `span` of the monotonic clock in which true time advanced by `elapsed`,
/// clamped to what is believable.
fn measured_rate(span: Duration, elapsed: Offset) -> i32 {
    let span = i128::try_from(span.as_nanos()).unwrap_or(i128::MAX).max(1);
    let ppm = (elapsed.0 - span) * PPM / span;
    i32::try_from(ppm.clamp(-i128::from(MAX_RATE_PPM), i128::from(MAX_RATE_PPM))).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use assert2::check;
    use rstest::rstest;

    use super::*;

    fn at(secs: f64) -> Instant {
        Instant::from_epoch(Duration::from_secs_f64(secs))
    }

    fn utc(secs: f64) -> UnixTime {
        UnixTime::from_epoch(Duration::from_secs_f64(1_700_000_000.0 + secs))
    }

    #[test]
    fn a_fix_steps_the_clock_and_says_by_how_much() {
        let mut clock = WallClock::from_rtc(at(10.0), utc(0.0));
        check!(!clock.is_fixed());
        check!(clock.at(at(40.0)) == utc(30.0));

        let correction = clock.fix(at(40.0), utc(32.5));
        check!(correction.step == Offset::between(utc(30.0), utc(32.5)));
        check!(correction.rate_ppm == 0);
        check!(clock.is_fixed());
        check!(clock.at(at(50.0)) == utc(42.5));
        check!(clock.at(at(30.0)) == utc(22.5), "before the anchor too");
    }

    #[test]
    fn the_rate_of_a_slow_monotonic_clock_is_learned_from_the_fixes_and_applied_between_them() {
        // The monotonic clock gains 1798.2 s while 1800 s of true time pass: 0.1% slow.
        let slow = |true_secs: f64| at(true_secs * 0.999);
        let mut clock = WallClock::from_fix(slow(0.0), utc(0.0));
        let correction = clock.fix(slow(1800.0), utc(1800.0));
        check!((correction.rate_ppm - 1001).abs() <= 2, "{correction:?}");
        check!(correction.step.abs() > Duration::from_millis(1700));

        let predicted = clock.at(slow(3600.0));
        let error = Offset::between(predicted, utc(3600.0)).abs();
        check!(error < Duration::from_millis(50), "{error:?}");

        let second = clock.fix(slow(3600.0), utc(3600.0));
        check!(second.step.abs() < Duration::from_millis(50));
    }

    #[test]
    fn fixes_seconds_apart_leave_the_rate_alone() {
        let mut clock = WallClock::from_fix(at(0.0), utc(0.0));
        clock.fix(at(60.0), utc(60.2));
        check!(clock.rate_ppm() == 0);
    }

    #[rstest]
    #[case::too_fast(1000.0, 1003.0, MAX_RATE_PPM)]
    #[case::too_slow(1000.0, 997.0, -MAX_RATE_PPM)]
    fn an_implausible_rate_is_clamped(#[case] monotonic: f64, #[case] truth: f64, #[case] want: i32) {
        let mut clock = WallClock::from_fix(at(0.0), utc(0.0));
        let correction = clock.fix(at(monotonic), utc(truth));
        check!(correction.rate_ppm == want, "{correction:?}");
        check!(!correction.rate_reset);
    }

    #[test]
    fn a_fix_far_from_the_prediction_resets_the_rate() {
        let mut clock = WallClock::from_fix(at(0.0), utc(0.0));
        clock.fix(at(1000.0), utc(1001.0));
        check!(clock.rate_ppm() != 0);

        let correction = clock.fix(at(2000.0), utc(2000.0 + 60.0));
        check!(correction.rate_reset);
        check!(correction.rate_ppm == 0);
        check!(clock.rate_ppm() == 0);
    }

    #[rstest]
    #[case(12_345, "+12.345 ms")]
    #[case(-500, "-0.500 ms")]
    #[case(-3_250_000, "-3.250 s")]
    #[case(7_200_000_000, "+7200.000 s")]
    fn offsets_print_with_their_sign_and_a_fitting_unit(#[case] micros: i64, #[case] text: &str) {
        let zero = UnixTime::from_epoch(Duration::from_secs(1_000_000));
        let size = Duration::from_micros(micros.unsigned_abs());
        let offset = if micros >= 0 {
            Offset::between(zero, zero.checked_add(size).unwrap())
        } else {
            Offset::between(zero.checked_add(size).unwrap(), zero)
        };
        check!(std::format!("{offset}") == text);
    }
}
