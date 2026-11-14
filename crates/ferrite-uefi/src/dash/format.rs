//! Durations, rates and sizes as the dashboard prints them.

use core::fmt::{self, Write as _};
use core::time::Duration;

use ferrite_mem::eta::{Confidence, Eta};
use heapless::String;

/// What the functions below return.
pub type Text = String<32>;

/// `args` formatted into a fixed-capacity string, cut where it does not fit.
#[must_use]
pub fn fixed<const N: usize>(args: fmt::Arguments<'_>) -> String<N> {
    let mut out = String::new();
    // Only a full buffer fails, and a cut line is what that means.
    out.write_fmt(args).ok();
    out
}

/// A number as decimal text.
#[must_use]
pub fn num(n: impl fmt::Display) -> String<24> {
    fixed(format_args!("{n}"))
}

/// `""` for one, else `"s"`.
#[must_use]
pub fn plural(n: impl TryInto<u64>) -> &'static str {
    if matches!(n.try_into(), Ok(1)) { "" } else { "s" }
}

/// A running clock: `m:ss`, or `h:mm:ss` from an hour on.
#[must_use]
pub fn clock(d: Duration) -> Text {
    let secs = d.as_secs();
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 { fixed(format_args!("{h}:{m:02}:{s:02}")) } else { fixed(format_args!("{m}:{s:02}")) }
}

/// Time left as the screens print it: to the second when it is measured (`12:34 left`), rounded and
/// marked when it is a guess (`~34 min left`, `~1 h 25 min left`).
#[must_use]
pub fn left(eta: Eta) -> Text {
    match eta.confidence {
        Confidence::Measured => fixed(format_args!("{} left", clock(eta.remaining))),
        Confidence::Estimated => fixed(format_args!("~{} left", approximate(eta.remaining))),
    }
}

/// A time to the minute, or to five minutes from an hour and a half on.
fn approximate(d: Duration) -> Text {
    let secs = d.as_secs();
    if secs < 90 {
        return fixed(format_args!("1 min"));
    }
    let minutes = (secs + 30) / 60;
    if minutes < 90 {
        return fixed(format_args!("{minutes} min"));
    }
    let rounded = (minutes + 2) / 5 * 5;
    match (rounded / 60, rounded % 60) {
        (h, 0) => fixed(format_args!("{h} h")),
        (h, m) => fixed(format_args!("{h} h {m:02} min")),
    }
}

/// A clock with every field: `hh:mm:ss`.
#[must_use]
pub fn hms(d: Duration) -> Text {
    let secs = d.as_secs();
    fixed(format_args!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60))
}

/// An address in hex with its digits in groups of four: `0x4_7A3C_1F08`.
#[must_use]
pub fn addr(value: u64) -> Text {
    let digits = fixed::<16>(format_args!("{value:X}"));
    let mut out = Text::new();
    // Cut only if the text does not fit, which 16 digits and their separators always do.
    out.push_str("0x").ok();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(4) {
            out.push('_').ok();
        }
        out.push(c).ok();
    }
    out
}

/// How long something took: hundredths under ten seconds, tenths under a minute, then a clock.
#[must_use]
pub fn took(d: Duration) -> Text {
    let millis = d.as_millis();
    let hundredths = (millis + 5) / 10;
    if hundredths < 1000 {
        return fixed(format_args!("{}.{:02} s", hundredths / 100, hundredths % 100));
    }
    let tenths = (millis + 50) / 100;
    if tenths < 600 {
        return fixed(format_args!("{}.{} s", tenths / 10, tenths % 10));
    }
    clock(Duration::from_secs(u64::try_from((millis + 500) / 1000).unwrap_or(u64::MAX)))
}

/// Bytes per second in decimal units, three significant digits.
#[must_use]
pub fn rate(bytes_per_sec: u64) -> Text {
    let mb = (bytes_per_sec + 500_000) / 1_000_000;
    if mb < 1000 {
        return fixed(format_args!("{mb} MB/s"));
    }
    let centi = (mb + 5) / 10;
    if centi < 1000 {
        return fixed(format_args!("{}.{:02} GB/s", centi / 100, centi % 100));
    }
    let deci = (mb + 50) / 100;
    fixed(format_args!("{}.{} GB/s", deci / 10, deci % 10))
}

/// A buffer size in the largest binary unit it is a whole number of, else MiB, or KiB under one.
#[must_use]
pub fn size(bytes: usize) -> Text {
    const KIB: usize = 1 << 10;
    const MIB: usize = 1 << 20;
    const GIB: usize = 1 << 30;
    if bytes >= GIB && bytes.is_multiple_of(GIB) {
        fixed(format_args!("{} GiB", bytes / GIB))
    } else if (KIB..MIB).contains(&bytes) {
        fixed(format_args!("{} KiB", bytes / KIB))
    } else {
        fixed(format_args!("{} MiB", bytes / MIB))
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case(0, "0.00 s")]
    #[case(1_234, "1.23 s")]
    #[case(9_994, "9.99 s")]
    #[case(9_995, "10.0 s")]
    #[case(59_949, "59.9 s")]
    #[case(59_950, "1:00")]
    #[case(3_599_600, "1:00:00")]
    fn took_switches_units_at_the_rounding_edge(#[case] millis: u64, #[case] expected: &str) {
        check!(took(Duration::from_millis(millis)) == expected);
    }

    #[rstest]
    #[case::measured_is_exact(754, Confidence::Measured, "12:34 left")]
    #[case::measured_hours(3_723, Confidence::Measured, "1:02:03 left")]
    #[case::guess_under_a_minute_and_a_half(89, Confidence::Estimated, "~1 min left")]
    #[case::guess_to_the_minute(90, Confidence::Estimated, "~2 min left")]
    #[case::guess_just_under_the_hour_and_a_half(5_369, Confidence::Estimated, "~89 min left")]
    #[case::guess_to_five_minutes(5_400, Confidence::Estimated, "~1 h 30 min left")]
    #[case::guess_whole_hours(7_199, Confidence::Estimated, "~2 h left")]
    #[case::guess_a_long_run(27_000 + 1_000, Confidence::Estimated, "~7 h 45 min left")]
    fn time_left_is_exact_only_when_measured(#[case] secs: u64, #[case] confidence: Confidence, #[case] want: &str) {
        check!(left(Eta { remaining: Duration::from_secs(secs), confidence }) == want);
    }

    #[rstest]
    #[case(59, "0:59")]
    #[case(61, "1:01")]
    #[case(3_661, "1:01:01")]
    fn clocks(#[case] secs: u64, #[case] expected: &str) {
        check!(clock(Duration::from_secs(secs)) == expected);
    }

    #[rstest]
    #[case(0x4_7a3c_1f08, "0x4_7A3C_1F08")]
    #[case(0x1000, "0x1000")]
    #[case(0x1_0000, "0x1_0000")]
    #[case(0, "0x0")]
    #[case(0xaaaa_aaaa_5555_5555, "0xAAAA_AAAA_5555_5555")]
    fn addresses_group_by_four(#[case] value: u64, #[case] expected: &str) {
        check!(addr(value) == expected);
    }

    #[rstest]
    #[case(1 << 30, "1 GiB")]
    #[case(128 << 20, "128 MiB")]
    #[case(512 << 10, "512 KiB")]
    fn sizes(#[case] bytes: usize, #[case] expected: &str) {
        check!(size(bytes) == expected);
    }

    #[rstest]
    #[case(999_499_999, "999 MB/s")]
    #[case(999_500_000, "1.00 GB/s")]
    #[case(12_345_000_000, "12.3 GB/s")]
    #[case(9_996_000_000, "10.0 GB/s")]
    #[case(1_005_000_000, "1.01 GB/s")]
    fn rates(#[case] bytes_per_sec: u64, #[case] expected: &str) {
        check!(rate(bytes_per_sec) == expected);
    }
}
