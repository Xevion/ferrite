//! What a run is for: some number of passes, a time budget, a first fault, any combination of them,
//! or nothing, in which case it goes on until stopped.
//!
//! The goal comes from the `[run]` section of `\EFI\ferrite\ferrite.toml` when the app starts and can
//! be set from the status page. When one of its parts is met the run ends the way `q` ends it.

use core::fmt;
use core::num::NonZeroU64;
use core::time::Duration;

use ferrite_config::{Compact, RunGoal};

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
}

impl From<RunGoal> for Goal {
    fn from(goal: RunGoal) -> Self {
        Self { passes: goal.passes, time: goal.time, stop_on_fault: goal.stop_on_fault }
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

#[cfg(test)]
mod tests {
    use alloc::format;

    use assert2::{assert, check};
    use rstest::rstest;

    use super::*;

    fn goal(passes: Option<u64>, secs: Option<u64>, stop_on_fault: bool) -> Goal {
        Goal { passes: passes.and_then(NonZeroU64::new), time: secs.map(Duration::from_secs), stop_on_fault }
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
