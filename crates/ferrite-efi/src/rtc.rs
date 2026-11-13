//! The firmware's real-time clock as time since the Unix epoch.

use core::time::Duration;

use uefi::runtime::{self, Time};

/// Days from 1970-01-01 to the civil date, proleptic Gregorian (Howard Hinnant's `days_from_civil`).
fn days_since_epoch(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The RTC reading as time since the Unix epoch, in UTC.
///
/// A time with an unspecified zone is taken as UTC; one with a zone is local time that far east of UTC,
/// as uefi's own time conversions read it. Dates before 1970 clamp to the epoch.
#[must_use]
pub fn unix_time(time: &Time) -> Duration {
    let days = days_since_epoch(i64::from(time.year()), i64::from(time.month()), i64::from(time.day()));
    let local =
        days * 86_400 + i64::from(time.hour()) * 3600 + i64::from(time.minute()) * 60 + i64::from(time.second());
    let utc = local - time.time_zone().map_or(0, |minutes| i64::from(minutes) * 60);
    Duration::new(u64::try_from(utc).unwrap_or(0), time.nanosecond())
}

/// Read the RTC.
pub fn now() -> uefi::Result<Duration> {
    runtime::get_time().map(|time| unix_time(&time))
}

#[cfg(test)]
mod tests {
    use assert2::check;
    use rstest::rstest;
    use uefi::runtime::{Daylight, TimeParams};

    use super::*;

    fn time(year: u16, month: u8, day: u8, hour: u8, zone: Option<i16>) -> Time {
        Time::new(TimeParams {
            year,
            month,
            day,
            hour,
            minute: 30,
            second: 15,
            nanosecond: 0,
            time_zone: zone,
            daylight: Daylight::empty(),
        })
        .expect("a valid time")
    }

    #[rstest]
    #[case(time(1970, 1, 1, 0, None), 30 * 60 + 15)]
    #[case(time(2000, 3, 1, 0, None), 951_868_800 + 30 * 60 + 15)]
    #[case(time(2024, 2, 29, 12, None), 1_709_208_000 + 30 * 60 + 15)]
    #[case(time(2026, 11, 12, 0, Some(480)), 1_794_441_600 - 8 * 3600 + 30 * 60 + 15)]
    fn civil_time_converts_to_unix_seconds(#[case] time: Time, #[case] seconds: u64) {
        check!(unix_time(&time).as_secs() == seconds);
    }
}
