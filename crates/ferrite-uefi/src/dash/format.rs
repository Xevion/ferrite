//! Durations, rates and sizes as the dashboard prints them.

use alloc::format;
use alloc::string::String;
use core::time::Duration;

/// A running clock: `m:ss`, or `h:mm:ss` from an hour on.
#[must_use]
pub fn clock(d: Duration) -> String {
    let secs = d.as_secs();
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

/// How long something took: hundredths under ten seconds, tenths under a minute, then a clock.
#[must_use]
pub fn took(d: Duration) -> String {
    let millis = d.as_millis();
    let hundredths = (millis + 5) / 10;
    if hundredths < 1000 {
        return format!("{}.{:02} s", hundredths / 100, hundredths % 100);
    }
    let tenths = (millis + 50) / 100;
    if tenths < 600 {
        return format!("{}.{} s", tenths / 10, tenths % 10);
    }
    clock(Duration::from_secs(u64::try_from((millis + 500) / 1000).unwrap_or(u64::MAX)))
}

/// Bytes per second in decimal units, three significant digits.
#[must_use]
pub fn rate(bytes_per_sec: u64) -> String {
    let mb = (bytes_per_sec + 500_000) / 1_000_000;
    if mb < 1000 {
        return format!("{mb} MB/s");
    }
    let centi = (mb + 5) / 10;
    if centi < 1000 {
        return format!("{}.{:02} GB/s", centi / 100, centi % 100);
    }
    let deci = (mb + 50) / 100;
    format!("{}.{} GB/s", deci / 10, deci % 10)
}

/// A buffer size in the largest binary unit it is a whole number of, else MiB.
#[must_use]
pub fn size(bytes: usize) -> String {
    const MIB: usize = 1 << 20;
    const GIB: usize = 1 << 30;
    if bytes >= GIB && bytes.is_multiple_of(GIB) {
        format!("{} GiB", bytes / GIB)
    } else {
        format!("{} MiB", bytes / MIB)
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
    #[case(59, "0:59")]
    #[case(61, "1:01")]
    #[case(3_661, "1:01:01")]
    fn clocks(#[case] secs: u64, #[case] expected: &str) {
        check!(clock(Duration::from_secs(secs)) == expected);
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
