//! The time of day as the screens and the status page show it: the machine's wall clock ([`WallClock`],
//! seeded from the RTC and corrected by NTP) in the configured zone and clock style.
//!
//! The clock and the zone are one shared [`Wall`]: the network corrects it as fixes arrive while the
//! screens read it. What a screen draws is a small `Copy` value ([`TimeLine`]), so a frame whose time
//! text is the same as the last one's is not redrawn.

use core::cell::Cell;
use core::time::Duration;

use ferrite_config::ClockFormat;
use ferrite_hal::{Instant, UnixTime, WallClock};
use ferrite_mem::eta::Eta;
use ferrite_net::sntp::Fix;
use jiff::Timestamp;
use jiff::civil::{Date, DateTime, Time, Weekday};
use jiff::tz::TimeZone;

use crate::dash::format::{Text, fixed};

/// What the clock is taken to read when it has no source at all, to place a server's timestamps in
/// their era: the start of 2026, which a binary running now is later than and 68 years short of.
const FALLBACK_PIVOT: UnixTime = UnixTime::from_epoch(Duration::from_hours(490_896));

/// Past this many days away, an end time is given as a date, not a weekday.
const WEEKDAY_DAYS: i64 = 6;

/// A time of day, to the second.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeOfDay {
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

impl TimeOfDay {
    fn of(time: Time) -> Self {
        let field = |n: i8| u8::try_from(n).unwrap_or(0);
        Self { hour: field(time.hour()), minute: field(time.minute()), second: field(time.second()) }
    }

    /// `01:27:43 AM` or `13:27:43`.
    #[must_use]
    pub fn text(self, format: ClockFormat) -> Text {
        let Self { minute, second, .. } = self;
        match format {
            ClockFormat::Twelve => {
                fixed(format_args!("{:02}:{minute:02}:{second:02} {}", self.hour12(), self.meridiem()))
            },
            ClockFormat::TwentyFour => fixed(format_args!("{:02}:{minute:02}:{second:02}", self.hour)),
        }
    }

    /// `03:14 AM` or `03:14`.
    #[must_use]
    pub fn text_to_the_minute(self, format: ClockFormat) -> Text {
        match format {
            ClockFormat::Twelve => fixed(format_args!("{:02}:{:02} {}", self.hour12(), self.minute, self.meridiem())),
            ClockFormat::TwentyFour => fixed(format_args!("{:02}:{:02}", self.hour, self.minute)),
        }
    }

    const fn hour12(self) -> u8 {
        match self.hour % 12 {
            0 => 12,
            hour => hour,
        }
    }

    const fn meridiem(self) -> &'static str {
        if self.hour < 12 { "AM" } else { "PM" }
    }
}

/// Which day an end time falls on, as far as the reader needs to be told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Day {
    Today,
    /// Within the next week.
    Weekday(Weekday),
    Date {
        month: u8,
        day: u8,
    },
}

/// When a run or a pass is expected to end, to the minute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ends {
    pub day: Day,
    pub time: TimeOfDay,
}

impl Ends {
    /// `03:14 AM`, `Sat 03:14 AM` or `Nov 21 03:14 AM`.
    #[must_use]
    pub fn text(self, format: ClockFormat) -> Text {
        let time = self.time.text_to_the_minute(format);
        match self.day {
            Day::Today => time,
            Day::Weekday(weekday) => fixed(format_args!("{} {time}", weekday_name(weekday))),
            Day::Date { month, day } => fixed(format_args!("{} {day} {time}", month_name(month))),
        }
    }
}

const fn weekday_name(weekday: Weekday) -> &'static str {
    match weekday {
        Weekday::Monday => "Mon",
        Weekday::Tuesday => "Tue",
        Weekday::Wednesday => "Wed",
        Weekday::Thursday => "Thu",
        Weekday::Friday => "Fri",
        Weekday::Saturday => "Sat",
        Weekday::Sunday => "Sun",
    }
}

fn month_name(month: u8) -> &'static str {
    const NAMES: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    NAMES.get(usize::from(month).wrapping_sub(1)).copied().unwrap_or("")
}

/// What a screen shows of the time for one frame: the time of day, and when the run's and the pass's
/// time left end, for the ones that have a time left to show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeLine {
    pub now: TimeOfDay,
    pub format: ClockFormat,
    pub overall: Option<Ends>,
    pub pass: Option<Ends>,
}

/// The wall clock, the zone to show it in and the clock's style.
pub struct Wall {
    clock: Cell<Option<WallClock>>,
    zone: TimeZone,
    format: ClockFormat,
}

impl Wall {
    /// `clock` is what the firmware's RTC gave, or `None` when it could not be read.
    #[must_use]
    pub const fn new(clock: Option<WallClock>, zone: TimeZone, format: ClockFormat) -> Self {
        Self { clock: Cell::new(clock), zone, format }
    }

    /// UTC at `at`, when the clock has a source.
    #[must_use]
    pub fn now(&self, at: Instant) -> Option<UnixTime> {
        self.clock.get().map(|clock| clock.at(at))
    }

    /// The time to place a server's timestamps by: the clock's, else a date this software is later than.
    #[must_use]
    pub fn pivot(&self, at: Instant) -> UnixTime {
        self.now(at).unwrap_or(FALLBACK_PIVOT)
    }

    /// Whether NTP has corrected the clock.
    #[must_use]
    pub fn is_synced(&self) -> bool {
        self.clock.get().is_some_and(|clock| clock.is_fixed())
    }

    #[must_use]
    pub fn zone_name(&self) -> &str {
        self.zone.iana_name().unwrap_or("UTC")
    }

    #[must_use]
    pub const fn format(&self) -> ClockFormat {
        self.format
    }

    /// Correct the clock by what NTP said, and log it.
    pub fn apply(&self, fix: &Fix) {
        let sample = &fix.sample;
        let (clock, change) = self.clock.get().map_or_else(
            || (WallClock::from_fix(sample.at, sample.utc), None),
            |mut clock| {
                let correction = clock.fix(sample.at, sample.utc);
                (clock, Some(correction))
            },
        );
        self.clock.set(Some(clock));
        let (server, stratum, delay) = (sample.server, sample.stratum, sample.delay);
        match change {
            Some(correction) => log::info!(
                "time: synced to {server} stratum {stratum}: offset {}, delay {delay:?}, best of {}, rate {:+} ppm{}",
                correction.step,
                fix.samples,
                correction.rate_ppm,
                if correction.rate_reset { " (reset)" } else { "" }
            ),
            None => log::info!(
                "time: synced to {server} stratum {stratum}: no clock before, delay {delay:?}, best of {}",
                fix.samples
            ),
        }
    }

    /// The time as the screens show it at `now`, with when the two times left end.
    #[must_use]
    pub fn line(&self, now: Instant, overall: Option<Eta>, pass: Option<Eta>) -> Option<TimeLine> {
        let clock = self.clock.get()?;
        let here = self.local(clock.at(now))?;
        let ends = |eta: Option<Eta>| eta.and_then(|eta| self.ends(&clock, now, eta.remaining, here.date()));
        Some(TimeLine {
            now: TimeOfDay::of(here.time()),
            format: self.format,
            overall: ends(overall),
            pass: ends(pass),
        })
    }

    /// UTC at the end of `left` from `now`, in Unix milliseconds.
    #[must_use]
    pub fn ends_unix_ms(&self, now: Instant, left: Duration) -> Option<u64> {
        Some(self.clock.get()?.at(now.checked_add(left)?).as_millis())
    }

    fn local(&self, utc: UnixTime) -> Option<DateTime> {
        let since = utc.since_epoch();
        let at =
            Timestamp::new(i64::try_from(since.as_secs()).ok()?, i32::try_from(since.subsec_nanos()).ok()?).ok()?;
        Some(self.zone.to_datetime(at))
    }

    fn ends(&self, clock: &WallClock, now: Instant, left: Duration, today: Date) -> Option<Ends> {
        // To the nearest minute, which is all the text says.
        let end = clock.at(now.checked_add(left)?).checked_add(Duration::from_secs(30))?;
        let there = self.local(end)?;
        let days = today.duration_until(there.date()).as_secs().div_euclid(86_400);
        let day = match days {
            ..=0 => Day::Today,
            1..=WEEKDAY_DAYS => Day::Weekday(there.date().weekday()),
            _ => Day::Date {
                month: u8::try_from(there.month()).unwrap_or(0),
                day: u8::try_from(there.day()).unwrap_or(0),
            },
        };
        Some(Ends { day, time: TimeOfDay::of(there.time()) })
    }
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};
    use ferrite_config::zone_named;
    use rstest::rstest;

    use super::*;

    const fn t(hour: u8, minute: u8, second: u8) -> TimeOfDay {
        TimeOfDay { hour, minute, second }
    }

    #[rstest]
    #[case::just_after_midnight(t(0, 5, 9), ClockFormat::Twelve, "12:05:09 AM")]
    #[case::the_example(t(1, 27, 43), ClockFormat::Twelve, "01:27:43 AM")]
    #[case::noon(t(12, 0, 0), ClockFormat::Twelve, "12:00:00 PM")]
    #[case::evening(t(23, 59, 59), ClockFormat::Twelve, "11:59:59 PM")]
    #[case::one_in_the_afternoon(t(13, 27, 43), ClockFormat::Twelve, "01:27:43 PM")]
    #[case::twenty_four(t(13, 27, 43), ClockFormat::TwentyFour, "13:27:43")]
    #[case::twenty_four_midnight(t(0, 0, 0), ClockFormat::TwentyFour, "00:00:00")]
    fn a_time_of_day_is_written_in_the_configured_style(
        #[case] time: TimeOfDay,
        #[case] format: ClockFormat,
        #[case] text: &str,
    ) {
        check!(time.text(format) == text);
    }

    fn wall(zone: &str, format: ClockFormat) -> Wall {
        // 2026-11-14 03:00:00 UTC, the clock's reading at the monotonic epoch.
        let seed = WallClock::from_rtc(Instant::default(), UnixTime::from_epoch(Duration::from_hours(498_507)));
        Wall::new(Some(seed), zone_named(zone).unwrap(), format)
    }

    fn eta(remaining: Duration) -> Eta {
        Eta { remaining, confidence: ferrite_mem::eta::Confidence::Estimated }
    }

    fn later(secs: u64) -> Instant {
        Instant::from_epoch(Duration::from_secs(secs))
    }

    #[test]
    fn the_time_of_day_is_the_clocks_in_the_zone_with_its_daylight_saving_rules() {
        // 03:00 UTC on 14 November is 21:00 the evening before in Chicago, which is on standard time then.
        let chicago = wall("America/Chicago", ClockFormat::Twelve);
        assert!(let Some(line) = chicago.line(later(0), None, None));
        check!(line.now.text(line.format) == "09:00:00 PM");

        // Eight months on it is summer time there, and the same UTC hour reads 22:00.
        let summer = later(60 * 60 * 24 * 243);
        assert!(let Some(line) = chicago.line(summer, None, None));
        check!(line.now == t(22, 0, 0));
        let utc = wall("UTC", ClockFormat::TwentyFour);
        assert!(let Some(line) = utc.line(later(0), None, None));
        check!(line.now == t(3, 0, 0));
    }

    #[rstest]
    #[case::later_today(60 * 25, "09:25 PM")]
    #[case::rounded_to_the_minute(60 * 25 + 29, "09:25 PM")]
    #[case::rounded_up(60 * 25 + 30, "09:26 PM")]
    #[case::past_midnight_is_tomorrow(3 * 3600 + 120, "Sat 12:02 AM")]
    #[case::six_days_on(6 * 86_400, "Thu 09:00 PM")]
    #[case::a_week_on(7 * 86_400, "Nov 20 09:00 PM")]
    #[case::a_month_on(30 * 86_400, "Dec 13 09:00 PM")]
    fn when_a_time_left_ends_says_the_day_only_when_it_is_not_today(#[case] left: u64, #[case] text: &str) {
        let chicago = wall("America/Chicago", ClockFormat::Twelve);
        let at_the_end = Some(eta(Duration::from_secs(left)));
        assert!(let Some(line) = chicago.line(later(0), at_the_end, at_the_end));
        assert!(let Some(ends) = line.overall);
        check!(ends.text(line.format) == text);
        check!(line.pass == line.overall);
    }

    #[test]
    fn the_twenty_four_hour_style_has_no_meridiem_on_an_end_time_either() {
        let berlin = wall("Europe/Berlin", ClockFormat::TwentyFour);
        assert!(let Some(line) = berlin.line(later(0), Some(eta(Duration::from_hours(2))), None));
        // 03:00 UTC is 04:00 in Berlin; two hours on is 06:00.
        check!(line.overall.unwrap().text(line.format) == "06:00");
        check!(line.pass.is_none(), "no time left, no end");
    }

    #[test]
    fn an_end_time_follows_the_clock_after_a_sync_moves_it() {
        let utc = wall("UTC", ClockFormat::TwentyFour);
        let before = utc.ends_unix_ms(later(10), Duration::from_secs(600)).unwrap();
        check!(before == (1_794_625_200 + 10 + 600) * 1000);

        let fix = Fix {
            sample: ferrite_net::sntp::Sample {
                server: smoltcp::wire::Ipv4Address::new(10, 0, 0, 1),
                stratum: 2,
                at: later(10),
                utc: UnixTime::from_epoch(Duration::from_hours(498_507 + 1)),
                delay: Duration::from_millis(30),
            },
            samples: 4,
        };
        utc.apply(&fix);
        check!(utc.is_synced());
        check!(utc.ends_unix_ms(later(10), Duration::from_secs(600)) == Some((1_794_625_200 + 3600 + 600) * 1000));
    }

    #[test]
    fn without_a_clock_there_is_no_time_to_show() {
        let wall = Wall::new(None, zone_named("UTC").unwrap(), ClockFormat::Twelve);
        check!(wall.line(later(0), Some(eta(Duration::from_mins(5))), None) == None);
        check!(wall.now(later(0)) == None && !wall.is_synced());
        check!(wall.ends_unix_ms(later(0), Duration::from_mins(5)) == None);
    }
}
