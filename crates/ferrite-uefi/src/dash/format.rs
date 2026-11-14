//! Durations, rates and sizes as the dashboard prints them.

use core::fmt::{self, Write as _};
use core::time::Duration;

use ferrite_mem::eta::BasisPoints;
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

/// Time left as the screens print it: `m:ss` under ten minutes, then whole minutes (`34 min`,
/// `1 h 35 min`). How far to trust it is told by the ink, not by the words.
#[must_use]
pub fn left(remaining: Duration) -> Text {
    let secs = remaining.as_secs();
    if secs < 600 {
        return fixed(format_args!("{} left", clock(remaining)));
    }
    let minutes = (secs + 30) / 60;
    match (minutes / 60, minutes % 60) {
        (0, m) => fixed(format_args!("{m} min left")),
        (h, 0) => fixed(format_args!("{h} h left")),
        (h, m) => fixed(format_args!("{h} h {m:02} min left")),
    }
}

/// A spread of rates as `±3%`, rounded up so that a figure over the unsteadiness threshold never
/// reads as the threshold itself.
#[must_use]
pub fn spread(spread: BasisPoints) -> Text {
    fixed(format_args!("\u{b1}{}%", spread.get().div_ceil(100)))
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

/// Bytes per second in decimal units, three significant digits; kilobytes under half a megabyte, as
/// a test of a few thousand words makes.
#[must_use]
pub fn rate(bytes_per_sec: u64) -> Text {
    let mb = (bytes_per_sec + 500_000) / 1_000_000;
    if mb == 0 {
        return fixed(format_args!("{} kB/s", (bytes_per_sec + 500) / 1000));
    }
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
    #[case::seconds(59, "0:59 left")]
    #[case::under_ten_minutes(599, "9:59 left")]
    #[case::ten_minutes(600, "10 min left")]
    #[case::rounds_to_the_nearest_minute_down(629, "10 min left")]
    #[case::rounds_to_the_nearest_minute_up(630, "11 min left")]
    #[case::almost_an_hour(3_569, "59 min left")]
    #[case::an_hour_by_rounding(3_570, "1 h left")]
    #[case::hour_and_minutes(5_700, "1 h 35 min left")]
    #[case::minutes_are_padded(3_900, "1 h 05 min left")]
    #[case::a_long_run(28_000, "7 h 47 min left")]
    fn time_left_is_to_the_second_under_ten_minutes_and_to_the_minute_after(#[case] secs: u64, #[case] want: &str) {
        check!(left(Duration::from_secs(secs)) == want);
    }

    #[rstest]
    #[case(0, "\u{b1}0%")]
    #[case(1, "\u{b1}1%")]
    #[case(300, "\u{b1}3%")]
    #[case(1000, "\u{b1}10%")]
    #[case(1001, "\u{b1}11%")]
    fn spreads_round_up(#[case] raw: u16, #[case] expected: &str) {
        check!(spread(BasisPoints::new(raw).unwrap()) == expected);
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
    #[case(20_480, "20 kB/s")]
    #[case(499_499, "499 kB/s")]
    #[case(499_500, "500 kB/s")]
    #[case(500_000, "1 MB/s")]
    #[case(999_499_999, "999 MB/s")]
    #[case(999_500_000, "1.00 GB/s")]
    #[case(12_345_000_000, "12.3 GB/s")]
    #[case(9_996_000_000, "10.0 GB/s")]
    #[case(1_005_000_000, "1.01 GB/s")]
    fn rates(#[case] bytes_per_sec: u64, #[case] expected: &str) {
        check!(rate(bytes_per_sec) == expected);
    }
}
