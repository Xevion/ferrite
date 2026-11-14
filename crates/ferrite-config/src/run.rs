//! What a run is for, and the time syntax the settings file and the status page share.

use core::fmt;
use core::num::NonZeroU64;
use core::time::Duration;

/// The shortest and longest time budget: below a second is a typo, and past a year is a number of
/// seconds that was not meant as one.
const MIN_TIME: Duration = Duration::from_secs(1);
const MAX_TIME: Duration = Duration::from_hours(365 * 24);

/// The validated `[run]` section: the first part met ends the run, and without any it goes on until
/// stopped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunGoal {
    /// Passes over the memory.
    pub passes: Option<NonZeroU64>,
    /// Wall time since the run started, pauses included.
    pub time: Option<Duration>,
    /// Stop at the first memory fault.
    pub stop_on_fault: bool,
}

/// Durations as runs of `<number><unit>`, unit `d`, `h`, `m` or `s`: `90s`, `2h`, `1h30m`. Between a
/// second and a year.
#[must_use]
pub fn parse_duration(value: &str) -> Option<Duration> {
    let mut total = Duration::ZERO;
    let mut rest = value;
    while !rest.is_empty() {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        let n: u64 = rest[..digits].parse().ok()?;
        let unit = rest[digits..].chars().next()?;
        let secs = match unit.to_ascii_lowercase() {
            'd' => 86_400,
            'h' => 3600,
            'm' => 60,
            's' => 1,
            _ => return None,
        };
        total = total.checked_add(Duration::from_secs(n.checked_mul(secs)?))?;
        rest = &rest[digits + unit.len_utf8()..];
    }
    (MIN_TIME..=MAX_TIME).contains(&total).then_some(total)
}

/// A duration as `1h30m`, the form [`parse_duration`] reads.
pub struct Compact(pub Duration);

impl fmt::Display for Compact {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let secs = self.0.as_secs();
        let (days, hours, mins, rest) = (secs / 86_400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
        if secs == 0 {
            return f.write_str("0s");
        }
        for (n, unit) in [(days, 'd'), (hours, 'h'), (mins, 'm'), (rest, 's')] {
            if n > 0 {
                write!(f, "{n}{unit}")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloc::format;

    use assert2::check;
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case("1s", 1)]
    #[case("1h30m", 5400)]
    #[case("1H30M15s", 5415)]
    #[case("2d", 172_800)]
    #[case("90m", 5400)]
    #[case("30m30m", 3600)]
    fn durations_add_up(#[case] text: &str, #[case] secs: u64) {
        check!(parse_duration(text) == Some(Duration::from_secs(secs)));
    }

    #[rstest]
    #[case::empty("")]
    #[case::no_unit("90")]
    #[case::no_number("h")]
    #[case::zero("0s")]
    #[case::beyond_a_year("400d")]
    #[case::overflow("99999999999999999999h")]
    #[case::spaced("1h 30m")]
    fn bad_durations_are_refused(#[case] text: &str) {
        check!(parse_duration(text) == None);
    }

    #[test]
    fn a_duration_prints_the_way_it_is_read() {
        for secs in [1, 59, 60, 3600, 5400, 5415, 90_061, 365 * 86_400] {
            let d = Duration::from_secs(secs);
            check!(parse_duration(&format!("{}", Compact(d))) == Some(d));
        }
    }
}
