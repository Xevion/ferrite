//! The clock policy over time: which ceiling each test runs under, from what the test measured.
//!
//! The app feeds an [`Observation`] every tick: the test running, its work done so far, and the
//! temperature and package power. In [`ClockMode::Auto`] and [`ClockMode::Calibrate`] each test is
//! first probed: each lower ceiling of the [`Ladder`] is run between two visits to the top one, each
//! visit a settle time and then a measured window, and its rate compared to theirs; the ceilings
//! take turns for a few rounds. A test probed to the end keeps
//! the lowest ceiling whose rate is within the tolerance of the top ceiling's; calibration keeps the highest
//! and only records. A probe cut short by the test ending resumes the next time the test runs.
//!
//! A temperature limit lowers whatever ceiling is chosen one step at a time while the package is at
//! or over it, and gives the steps back once it has stayed well under for a while.

use alloc::vec::Vec;
use core::fmt;
use core::time::Duration;

use ferrite_hal::Instant;

use crate::control::{Bound, Epp, Levels, Request};
use crate::policy::{ClockMode, Policy};
use crate::units::{Mhz, MicroJoules, MilliWatts};

/// How many ceilings a ladder has at most.
pub const MAX_STEPS: usize = 6;
/// Degrees under the limit the package must stay before a lowered ceiling comes back up.
const COOL_MARGIN: i16 = 5;
/// How long the package must stay under the limit less [`COOL_MARGIN`] before each step back up.
const COOL_HOLD: Duration = Duration::from_secs(10);
/// How long a lowered ceiling is kept at least, so the temperature has time to answer.
const HEAT_HOLD: Duration = Duration::from_secs(3);

/// What the app saw this tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Observation {
    pub now: Instant,
    /// The test running, by name.
    pub test: &'static str,
    /// Work units of the test done so far, increasing while it runs.
    pub done: u64,
    pub celsius: Option<i16>,
    /// The package's energy counter so far, when there is one.
    pub energy: Option<MicroJoules>,
}

/// The ceilings a probe tries, highest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ladder {
    steps: [Bound; MAX_STEPS],
    len: usize,
}

impl Ladder {
    /// From the highest level down to the nominal (base) one in even steps, as clocks where the
    /// levels have a scale (so cores whose units differ get the same clock) and as levels otherwise.
    /// Below the base clock every memory test measured on the X1 Carbon ran 15% or more slower.
    #[must_use]
    pub fn from_levels(levels: &Levels, count: usize) -> Self {
        let count = count.clamp(1, MAX_STEPS);
        let (top, bottom) = (levels.highest, levels.nominal.max(levels.lowest).min(levels.highest));
        let mut steps = [Bound::Highest; MAX_STEPS];
        let mut len = 0;
        for i in 0..count {
            let span = u32::from(top - bottom);
            let level = top - (span * i as u32 / (count as u32 - 1).max(1)) as u8;
            let bound =
                if i == 0 { Bound::Highest } else { levels.mhz(level).map_or(Bound::Level(level), Bound::Clock) };
            if len == 0 || steps[len - 1] != bound {
                steps[len] = bound;
                len += 1;
            }
        }
        Self { steps, len }
    }

    #[must_use]
    pub fn steps(&self) -> &[Bound] {
        &self.steps[..self.len]
    }

    fn get(&self, index: usize) -> Bound {
        self.steps[index.min(self.len - 1)]
    }
}

/// How long each probe visit lasts, and how many times each lower ceiling is visited.
///
/// A test's work rate changes through its phases, so a ceiling measured at one point is tied to
/// the part of the test it ran in. Each lower ceiling is therefore bracketed: measured between two
/// visits to the top ceiling, and compared to their average, so the phase cancels out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    /// Time after a ceiling change before measuring, for the clock and the work rate to settle.
    pub settle: Duration,
    pub window: Duration,
    pub rounds: usize,
}

impl Timing {
    pub const AUTO: Self = Self { settle: Duration::from_millis(300), window: Duration::from_secs(1), rounds: 2 };
    pub const CALIBRATE: Self = Self { settle: Duration::from_millis(500), window: Duration::from_secs(2), rounds: 3 };
}

/// What one ceiling measured for one test, summed over its visits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Step {
    /// Work units done while measured.
    pub done: u64,
    pub time: Duration,
    /// Package energy used while measured; `None` without an energy counter.
    pub energy: Option<MicroJoules>,
    pub visits: u32,
    pub peak_celsius: Option<i16>,
    /// The sum over visits of the rate relative to the top ceiling's around it, in millionths.
    relative_sum: u64,
    relative_count: u32,
}

impl Step {
    /// Work units per second, averaged over the visits.
    #[must_use]
    pub fn rate(&self) -> u64 {
        let micros = self.time.as_micros().max(1);
        u64::try_from(u128::from(self.done) * 1_000_000 / micros).unwrap_or(u64::MAX)
    }

    #[must_use]
    pub fn power(&self) -> Option<MilliWatts> {
        self.energy?.per(self.time)
    }

    /// The rate relative to the top ceiling measured around each visit, in millionths; `None`
    /// until a visit has been bracketed.
    #[must_use]
    pub fn relative(&self) -> Option<u64> {
        (self.relative_count > 0).then(|| self.relative_sum / u64::from(self.relative_count))
    }

    fn add(&mut self, done: u64, time: Duration, energy: Option<MicroJoules>, peak: Option<i16>) {
        self.energy = match (self.visits, self.energy, energy) {
            (0, _, e) => e,
            (_, Some(a), Some(b)) => Some(a.saturating_add(b)),
            _ => None,
        };
        self.done += done;
        self.time += time;
        self.visits += 1;
        self.peak_celsius = self.peak_celsius.max(peak);
    }
}

/// What a test measured, and the ceiling it was given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub test: &'static str,
    /// Per ladder step.
    pub steps: [Step; MAX_STEPS],
    /// Visits made so far: the top ceiling, then a lower one, then the top again, and so on.
    pub visits: usize,
    /// The chosen ladder index, once every round is done.
    pub chosen: Option<usize>,
    /// The top ceiling's last rate, and a lower visit's step and rate waiting for the next one.
    last_top: Option<u64>,
    pending: Option<(usize, u64)>,
}

impl Record {
    fn new(test: &'static str) -> Self {
        Self { test, steps: [Step::default(); MAX_STEPS], visits: 0, chosen: None, last_top: None, pending: None }
    }

    /// The step to visit next while rounds remain: the top on even visits, the lower ones in
    /// turn on odd visits, ending on the top so the last lower visit is bracketed.
    const fn next_visit(&self, len: usize, rounds: usize) -> Option<usize> {
        let lower = len - 1;
        if self.visits > 2 * lower * rounds {
            return None;
        }
        if self.visits.is_multiple_of(2) || lower == 0 { Some(0) } else { Some(1 + (self.visits / 2) % lower) }
    }

    /// Take a visit's rate: a lower one waits for the next top visit; a top one closes the
    /// lower visit before it against the average of the two around it.
    fn bracket(&mut self, step: usize, rate: u64) {
        if step != 0 {
            self.pending = self.last_top.map(|_| (step, rate));
            return;
        }
        if let (Some((lower, lower_rate)), Some(before)) = (self.pending.take(), self.last_top) {
            let around = before.midpoint(rate);
            if let Some(relative) = (u128::from(lower_rate) * 1_000_000).checked_div(u128::from(around)) {
                self.steps[lower].relative_sum += u64::try_from(relative).unwrap_or(u64::MAX);
                self.steps[lower].relative_count += 1;
            }
        }
        self.last_top = Some(rate);
    }
}

/// The lowest step whose rate, relative to the top ceiling around it, is no more than `tolerance`
/// percent below it; the bottom when nothing moved, since a test with no measurable work is waiting
/// and any ceiling serves it. A step faster than the top (the top held back by heat) passes too.
fn choose(steps: &[Step], tolerance: u8) -> usize {
    if steps.first().is_none_or(|top| top.rate() == 0) {
        return steps.len().saturating_sub(1);
    }
    let relative: Vec<u64> =
        steps.iter().enumerate().map(|(i, s)| if i == 0 { 1_000_000 } else { s.relative().unwrap_or(0) }).collect();
    let floor = 1_000_000 * u128::from(100 - tolerance.min(100)) / 100;
    relative.iter().rposition(|&r| u128::from(r) >= floor).unwrap_or(0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Waiting for the clock to settle under `step` until `until`.
    Settling { step: usize, until: Instant },
    /// Measuring `step` from the work, energy and time at its start.
    Measuring { step: usize, start: Instant, done: u64, energy: Option<MicroJoules>, peak: Option<i16> },
    /// Running at the chosen ceiling.
    Holding,
}

/// The heat guard's lowering of the ceiling, in ladder steps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Guard {
    steps: usize,
    since: Option<Instant>,
}

/// The clock policy's state across ticks.
#[derive(Clone, Debug)]
pub struct Governor {
    policy: Policy,
    ladder: Ladder,
    timing: Timing,
    records: Vec<Record>,
    test: Option<&'static str>,
    phase: Phase,
    guard: Guard,
    /// The ceiling last asked for, as a ladder index; `None` before the first request.
    asked: Option<usize>,
}

impl Governor {
    #[must_use]
    pub fn new(policy: Policy, ladder: Ladder) -> Self {
        let timing = if policy.clock == ClockMode::Calibrate { Timing::CALIBRATE } else { Timing::AUTO };
        Self {
            policy,
            ladder,
            timing,
            records: Vec::new(),
            test: None,
            phase: Phase::Holding,
            guard: Guard::default(),
            asked: None,
        }
    }

    #[must_use]
    pub const fn with_timing(mut self, timing: Timing) -> Self {
        self.timing = timing;
        self
    }

    #[must_use]
    pub const fn ladder(&self) -> &Ladder {
        &self.ladder
    }

    #[must_use]
    pub fn records(&self) -> &[Record] {
        &self.records
    }

    /// Take in this tick; returns the request to make when the ceiling changes.
    pub fn observe(&mut self, obs: Observation) -> Option<Request> {
        if self.policy.clock == ClockMode::Firmware {
            return None;
        }
        if self.test != Some(obs.test) {
            self.start_test(obs);
        }
        if matches!(self.policy.clock, ClockMode::Auto | ClockMode::Calibrate) {
            self.advance(obs);
        }
        self.heat(obs);
        let wanted = self.wanted();
        (self.asked != Some(wanted)).then(|| {
            self.asked = Some(wanted);
            Request { floor: Bound::Lowest, ceiling: self.ladder.get(wanted), preference: Epp::PERFORMANCE }
        })
    }

    /// The ladder index the policy wants now, the heat guard included.
    fn wanted(&self) -> usize {
        let base = match (self.policy.clock, self.phase) {
            (ClockMode::Auto | ClockMode::Calibrate, Phase::Settling { step, .. } | Phase::Measuring { step, .. }) => {
                step
            },
            (ClockMode::Auto, Phase::Holding) => self.record().and_then(|r| r.chosen).unwrap_or(0),
            _ => 0,
        };
        (base + self.guard.steps).min(self.ladder.steps().len() - 1)
    }

    fn record(&self) -> Option<&Record> {
        let test = self.test?;
        self.records.iter().find(|r| r.test == test)
    }

    fn record_mut(&mut self) -> Option<&mut Record> {
        let test = self.test?;
        self.records.iter_mut().find(|r| r.test == test)
    }

    fn start_test(&mut self, obs: Observation) {
        self.test = Some(obs.test);
        if self.record().is_none() {
            self.records.push(Record::new(obs.test));
        }
        self.phase = Phase::Holding;
        self.next_step(obs.now);
    }

    /// Settle into the next unmeasured step, or hold when there is none.
    fn next_step(&mut self, now: Instant) {
        let len = self.ladder.steps().len();
        let rounds = self.timing.rounds;
        let next = self.record().and_then(|r| r.next_visit(len, rounds));
        self.phase = match next {
            Some(step) if matches!(self.policy.clock, ClockMode::Auto | ClockMode::Calibrate) => {
                Phase::Settling { step, until: now.checked_add(self.timing.settle).unwrap_or(now) }
            },
            _ => Phase::Holding,
        };
    }

    fn advance(&mut self, obs: Observation) {
        match self.phase {
            Phase::Settling { step, until } if obs.now >= until => {
                self.phase =
                    Phase::Measuring { step, start: obs.now, done: obs.done, energy: obs.energy, peak: obs.celsius };
            },
            Phase::Measuring { step, start, done, energy, peak } => {
                let peak = peak.max(obs.celsius);
                let elapsed = obs.now.saturating_duration_since(start);
                if elapsed < self.timing.window {
                    self.phase = Phase::Measuring { step, start, done, energy, peak };
                    return;
                }
                let used = match (energy, obs.energy) {
                    (Some(a), Some(b)) => Some(MicroJoules(b.0.saturating_sub(a.0))),
                    _ => None,
                };
                let tolerance = self.policy.tolerance.percent();
                let (len, rounds) = (self.ladder.steps().len(), self.timing.rounds);
                if let Some(record) = self.record_mut() {
                    let work = obs.done.saturating_sub(done);
                    record.steps[step].add(work, elapsed, used, peak);
                    let micros = elapsed.as_micros().max(1);
                    record.bracket(step, u64::try_from(u128::from(work) * 1_000_000 / micros).unwrap_or(u64::MAX));
                    record.visits += 1;
                    if record.next_visit(len, rounds).is_none() {
                        record.chosen = Some(choose(&record.steps[..len], tolerance));
                    }
                }
                self.next_step(obs.now);
            },
            _ => {},
        }
    }

    fn heat(&mut self, obs: Observation) {
        let (Some(limit), Some(celsius)) = (self.policy.max_temp, obs.celsius) else { return };
        let limit = limit.celsius();
        let held = self.guard.since.map_or(Duration::MAX, |s| obs.now.saturating_duration_since(s));
        let top = self.ladder.steps().len() - 1;
        if celsius >= limit {
            if held >= HEAT_HOLD && self.guard.steps < top {
                self.guard = Guard { steps: self.guard.steps + 1, since: Some(obs.now) };
            }
        } else if celsius >= limit - COOL_MARGIN {
            // Near the limit: hold the lowered ceiling and restart the wait to raise it.
            if self.guard.steps > 0 {
                self.guard.since = Some(obs.now);
            }
        } else if self.guard.steps > 0 && held >= COOL_HOLD {
            self.guard = Guard { steps: self.guard.steps - 1, since: Some(obs.now) };
        }
    }
}

impl fmt::Display for Record {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:", self.test)?;
        for (i, step) in self.steps.iter().enumerate().filter(|(_, s)| s.visits > 0) {
            write!(f, " [{i}] {}/s", step.rate())?;
            if let Some(r) = step.relative() {
                write!(f, " ({}.{}%)", r / 10_000, r / 1000 % 10)?;
            }
            if let Some(power) = step.power() {
                write!(f, " {power}")?;
            }
            if let Some(c) = step.peak_celsius {
                write!(f, " {c}C")?;
            }
        }
        match self.chosen {
            Some(i) => write!(f, "; chose step {i}"),
            None => write!(f, "; {} visits so far", self.visits),
        }
    }
}

impl fmt::Display for Ladder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, step) in self.steps().iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "[{i}] {step}")?;
        }
        Ok(())
    }
}

/// The clock a ladder step names, where it names one.
#[must_use]
pub fn step_clock(levels: &Levels, step: Bound) -> Option<Mhz> {
    match step {
        Bound::Clock(mhz) => Some(mhz),
        Bound::Highest => levels.mhz(levels.highest),
        Bound::Level(level) => levels.mhz(level),
        _ => None,
    }
}
