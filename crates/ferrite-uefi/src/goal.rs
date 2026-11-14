//! What a run is for: some number of passes, a time budget, a first fault, any combination of them,
//! or nothing, in which case it goes on until stopped.
//!
//! The goal comes from `\EFI\ferrite\run.env` when the app starts and can be set from the status
//! page. When one of its parts is met the run ends the way `q` ends it.

use core::fmt;
use core::num::NonZeroU64;
use core::time::Duration;

use ferrite_efi::volume::{self, ReadError};
use snafu::Snafu;

/// Where the goal lives on the boot volume.
pub const CONFIG_PATH: &str = "\\EFI\\ferrite\\run.env";

pub const PASSES_KEY: &str = "RUN_PASSES";
pub const TIME_KEY: &str = "RUN_TIME";
pub const STOP_ON_FAULT_KEY: &str = "RUN_STOP_ON_FAULT";

/// The shortest and longest time budget: below a second is a typo, and past a year is a number of
/// seconds that was not meant as one.
const MIN_TIME: Duration = Duration::from_secs(1);
const MAX_TIME: Duration = Duration::from_hours(365 * 24);

/// When a run is done: the first part of the goal that is met ends it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Goal {
    /// Passes over the memory, each with its rotation of the firmware's reserve.
    pub passes: Option<NonZeroU64>,
    /// Wall time since the run started, pauses included.
    pub time: Option<Duration>,
    /// Stop at the first memory fault.
    pub stop_on_fault: bool,
}

/// Which part of the goal was met.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Met {
    Passes(NonZeroU64),
    Time(Duration),
    Fault,
}

impl fmt::Display for Met {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Passes(n) => write!(f, "{n} passes done"),
            Self::Time(d) => write!(f, "{} elapsed", Compact(*d)),
            Self::Fault => f.write_str("a fault was found"),
        }
    }
}

impl Goal {
    pub const UNLIMITED: Self = Self { passes: None, time: None, stop_on_fault: false };

    /// Whether nothing can end the run but a stop request.
    #[must_use]
    pub const fn is_unlimited(&self) -> bool {
        self.passes.is_none() && self.time.is_none() && !self.stop_on_fault
    }

    /// The part of the goal that is met when `settled` passes are done (each with its reserve
    /// rotation), `elapsed` time has gone by and `faults` words have been found wrong.
    #[must_use]
    pub fn met(&self, settled: u64, elapsed: Duration, faults: u64) -> Option<Met> {
        if self.stop_on_fault && faults > 0 {
            return Some(Met::Fault);
        }
        if let Some(n) = self.passes.filter(|n| settled >= n.get()) {
            return Some(Met::Passes(n));
        }
        self.time.filter(|t| elapsed >= *t).map(Met::Time)
    }

    /// The goal in the settings text of `text`: `RUN_PASSES`, `RUN_TIME` and `RUN_STOP_ON_FAULT`, each
    /// optional. Other lines are ignored unless they start with `RUN_`; a key given twice takes its
    /// last line; one outer pair of double quotes is stripped.
    pub fn from_env(text: &[u8]) -> Result<Self, GoalError> {
        let text = core::str::from_utf8(text).map_err(|_| GoalError::Encoding)?;
        let mut goal = Self::UNLIMITED;
        for (index, line) in text.split('\n').enumerate() {
            let line_no = index + 1;
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line.split_once('=').ok_or(GoalError::Line { line: line_no })?;
            let (key, value) = (key.trim(), value.trim());
            let value = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(value);
            let bad = |key, problem| GoalError::Value { line: line_no, key, problem };
            match key {
                PASSES_KEY => goal.passes = Some(parse_passes(value).ok_or_else(|| bad(PASSES_KEY, Problem::Passes))?),
                TIME_KEY => goal.time = Some(parse_duration(value).ok_or_else(|| bad(TIME_KEY, Problem::Time))?),
                STOP_ON_FAULT_KEY => {
                    goal.stop_on_fault = parse_flag(value).ok_or_else(|| bad(STOP_ON_FAULT_KEY, Problem::Flag))?;
                },
                other if other.starts_with("RUN_") => return Err(GoalError::UnknownKey { line: line_no }),
                _ => {},
            }
        }
        Ok(goal)
    }
}

impl fmt::Display for Goal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_unlimited() {
            return f.write_str("until stopped");
        }
        let mut first = true;
        let mut sep = |f: &mut fmt::Formatter<'_>| {
            let out = if first { Ok(()) } else { f.write_str(", ") };
            first = false;
            out
        };
        if let Some(n) = self.passes {
            sep(f)?;
            write!(f, "{n} pass{}", if n.get() == 1 { "" } else { "es" })?;
        }
        if let Some(t) = self.time {
            sep(f)?;
            write!(f, "{}", Compact(t))?;
        }
        if self.stop_on_fault {
            sep(f)?;
            f.write_str("first fault")?;
        }
        Ok(())
    }
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

fn parse_passes(value: &str) -> Option<NonZeroU64> {
    value.parse().ok()
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

fn parse_flag(value: &str) -> Option<bool> {
    const YES: [&str; 4] = ["1", "true", "yes", "on"];
    const NO: [&str; 4] = ["0", "false", "no", "off"];
    let is = |words: [&str; 4]| words.into_iter().any(|w| w.eq_ignore_ascii_case(value));
    if is(YES) {
        Some(true)
    } else if is(NO) {
        Some(false)
    } else {
        None
    }
}

/// What is wrong with a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Problem {
    Passes,
    Time,
    Flag,
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Passes => "expected a whole number of passes, at least 1",
            Self::Time => "expected a time such as 90m, 2h or 1h30m, between 1 second and a year",
            Self::Flag => "expected 1 or 0 (or yes, no, true, false, on, off)",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Snafu)]
pub enum GoalError {
    #[snafu(display("not valid UTF-8"))]
    Encoding,
    #[snafu(display("line {line} is not KEY=VALUE"))]
    Line { line: usize },
    #[snafu(display("line {line} has an unknown RUN_ key"))]
    UnknownKey { line: usize },
    #[snafu(display("line {line}: {key}: {problem}"))]
    Value { line: usize, key: &'static str, problem: Problem },
}

/// The goal the settings on the boot volume describe; none (the run goes on until stopped), with a
/// log line saying why, when there is no file or it is not usable.
#[must_use]
pub fn load() -> Goal {
    from_file(volume::read_file(CONFIG_PATH))
}

fn from_file(file: Result<alloc::vec::Vec<u8>, ReadError>) -> Goal {
    let text = match file {
        Ok(text) => text,
        Err(ReadError::NotFound { .. }) => {
            log::info!("run: no {CONFIG_PATH}, running until stopped");
            return Goal::UNLIMITED;
        },
        Err(e) => {
            log::warn!("run: {e}, running until stopped");
            return Goal::UNLIMITED;
        },
    };
    match Goal::from_env(&text) {
        Ok(goal) => {
            log::info!("run: goal {goal}");
            goal
        },
        Err(e) => {
            log::warn!("run: {CONFIG_PATH}: {e}, running until stopped");
            Goal::UNLIMITED
        },
    }
}

#[cfg(test)]
mod tests {
    use alloc::format;

    use assert2::{assert, check};
    use rstest::rstest;

    use super::*;

    fn goal(passes: Option<u64>, secs: Option<u64>, stop_on_fault: bool) -> Goal {
        Goal { passes: passes.and_then(NonZeroU64::new), time: secs.map(Duration::from_secs), stop_on_fault }
    }

    #[rstest]
    #[case::nothing("", Goal::UNLIMITED)]
    #[case::comments("# no limit\n\n  # still none\n", Goal::UNLIMITED)]
    #[case::passes("RUN_PASSES=3", goal(Some(3), None, false))]
    #[case::time("RUN_TIME=2h", goal(None, Some(7200), false))]
    #[case::fault("RUN_STOP_ON_FAULT=1", goal(None, None, true))]
    #[case::all_three("RUN_PASSES=2\nRUN_TIME=1h30m\nRUN_STOP_ON_FAULT=yes\n", goal(Some(2), Some(5400), true))]
    #[case::quoted_and_spaced("  RUN_TIME = \"45m\" \r\nRUN_STOP_ON_FAULT=Off", goal(None, Some(2700), false))]
    #[case::last_wins("RUN_PASSES=5\nRUN_PASSES=1", goal(Some(1), None, false))]
    #[case::other_lines_ignored("NOTIFY_URL=http://h/t\nWPA_SSID=x\nRUN_PASSES=4", goal(Some(4), None, false))]
    fn goals(#[case] text: &str, #[case] want: Goal) {
        check!(Goal::from_env(text.as_bytes()) == Ok(want));
    }

    #[rstest]
    #[case::not_a_pair("RUN_PASSES 3", GoalError::Line { line: 1 })]
    #[case::unknown_key("RUN_PASSES=1\nRUN_FOREVER=1", GoalError::UnknownKey { line: 2 })]
    #[case::zero_passes("RUN_PASSES=0", GoalError::Value { line: 1, key: PASSES_KEY, problem: Problem::Passes })]
    #[case::word_passes("RUN_PASSES=many", GoalError::Value { line: 1, key: PASSES_KEY, problem: Problem::Passes })]
    #[case::negative_passes("RUN_PASSES=-1", GoalError::Value { line: 1, key: PASSES_KEY, problem: Problem::Passes })]
    #[case::no_unit("\nRUN_TIME=90", GoalError::Value { line: 2, key: TIME_KEY, problem: Problem::Time })]
    #[case::no_number("RUN_TIME=h", GoalError::Value { line: 1, key: TIME_KEY, problem: Problem::Time })]
    #[case::zero_time("RUN_TIME=0s", GoalError::Value { line: 1, key: TIME_KEY, problem: Problem::Time })]
    #[case::beyond_a_year("RUN_TIME=400d", GoalError::Value { line: 1, key: TIME_KEY, problem: Problem::Time })]
    #[case::overflow("RUN_TIME=99999999999999999999h", GoalError::Value { line: 1, key: TIME_KEY, problem: Problem::Time })]
    #[case::bad_flag("RUN_STOP_ON_FAULT=maybe", GoalError::Value { line: 1, key: STOP_ON_FAULT_KEY, problem: Problem::Flag })]
    fn broken_files_name_the_line_and_key(#[case] text: &str, #[case] error: GoalError) {
        check!(Goal::from_env(text.as_bytes()) == Err(error));
    }

    #[test]
    fn a_file_that_is_not_text_is_refused() {
        check!(Goal::from_env(b"RUN_PASSES=\xff") == Err(GoalError::Encoding));
    }

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

    #[test]
    fn a_duration_prints_the_way_it_is_read() {
        for secs in [1, 59, 60, 3600, 5400, 5415, 90_061, 365 * 86_400] {
            let d = Duration::from_secs(secs);
            check!(parse_duration(&format!("{}", Compact(d))) == Some(d));
        }
    }

    #[test]
    fn the_first_part_met_ends_the_run() {
        let g = goal(Some(3), Some(7200), true);
        check!(g.met(2, Duration::from_secs(7199), 0).is_none());
        assert!(let Some(Met::Passes(n)) = g.met(3, Duration::from_secs(10), 0));
        check!(n.get() == 3);
        check!(g.met(1, Duration::from_secs(7200), 0) == Some(Met::Time(Duration::from_secs(7200))));
        check!(g.met(0, Duration::ZERO, 1) == Some(Met::Fault));
        check!(
            !goal(Some(3), None, false).stop_on_fault && goal(Some(3), None, false).met(0, Duration::ZERO, 9).is_none()
        );
        check!(Goal::UNLIMITED.met(1_000, Duration::from_secs(1 << 30), 1_000).is_none());
    }

    #[rstest]
    #[case(Goal::UNLIMITED, "until stopped")]
    #[case(goal(Some(1), None, false), "1 pass")]
    #[case(goal(Some(3), Some(5400), true), "3 passes, 1h30m, first fault")]
    fn goals_describe_themselves(#[case] g: Goal, #[case] text: &str) {
        check!(format!("{g}") == text);
    }
}
