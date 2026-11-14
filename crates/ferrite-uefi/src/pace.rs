//! How the run is going, as time: what the sweep tells the screen, the status page and the log
//! besides which test it is on.
//!
//! The estimating itself is `ferrite_mem::eta`. This is its seat in the app: one [`Lane`] for the
//! sweep's own memory and one for the reserve's rotation after each pass, each with its estimator,
//! rate meter and the priors the timings file holds for its setup; the goal; and the audit of how
//! the predictions came out. [`Outlook`] is the part of it a screen needs, small and `Copy` so a
//! change in it is a change in the status.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::time::Duration;

use ferrite_hal::Instant;
use ferrite_mem::eta::{Confidence, Estimator, Eta, Forecast, Position, RateMeter};
use ferrite_mem::suite::{Schedule, TestKind};

use crate::goal::{Compact, Goal};
use crate::machine::Machine;
use crate::sweep::Scope;
use crate::timings::{Disk, Key, Store};

/// How often the log gets a line of what is predicted.
const AUDIT_EVERY: Duration = Duration::from_secs(30);
/// How often the rate on show is replaced: it is measured over seconds, and a figure that changed
/// with every frame would make the status page render its document as often.
const RATE_EVERY: Duration = Duration::from_millis(500);

/// Round `d` to whole seconds, so a figure that is shown to the second changes once a second.
fn whole(d: Duration) -> Duration {
    Duration::from_secs(u64::try_from((d.as_millis() + 500) / 1000).unwrap_or(u64::MAX))
}

fn rounded(eta: Eta) -> Eta {
    Eta { remaining: whole(eta.remaining), ..eta }
}

/// What a screen needs to say how the run is going.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Outlook {
    pub goal: Goal,
    /// Passes done together with their reserve rotation, which is when a pass counts for the goal.
    pub settled: u64,
    /// The memory under test is the firmware's reserve, between passes.
    pub reserve: bool,
    /// Bytes of work per second over the last few seconds, to the megabyte.
    pub rate: Option<u64>,
    /// Where the running test is: touching memory or in one of its waits, with the time left in it.
    pub position: Position,
    /// Time left in the running test and how far through it is, per mille.
    pub test_left: Option<Duration>,
    pub test_permille: Option<u16>,
    /// Time left in the pass (in the reserve's pass, during its rotation), when fit to show.
    pub pass: Option<Eta>,
    /// How far through the pass it is by time, per mille.
    pub pass_permille: Option<u16>,
    /// A whole pass with its reserve rotation, as predicted, and how much of it is left.
    pub cycle: Option<Duration>,
    pub cycle_left: Option<Eta>,
    /// How many of the pass's tests have a time on record, and how many there are.
    pub known: (usize, usize),
}

impl Outlook {
    pub const NONE: Self = Self {
        goal: Goal::UNLIMITED,
        settled: 0,
        reserve: false,
        rate: None,
        position: Position::NONE,
        test_left: None,
        test_permille: None,
        pass: None,
        pass_permille: None,
        cycle: None,
        cycle_left: None,
        known: (0, 0),
    };

    /// The run measured against its goal, `elapsed` after it started. `pass_percent` is the pass's
    /// share of its words, which stands in for time until there is a prediction.
    #[must_use]
    pub fn overall(&self, elapsed: Duration, pass_percent: u16) -> Overall {
        let mut fill: Option<u32> = None;
        let mut left: Option<Eta> = None;
        let mut take = |f: u32, l: Option<Eta>| {
            fill = Some(fill.map_or(f, |old| old.max(f)));
            left = match (left, l) {
                (Some(a), Some(b)) => Some(if b.remaining < a.remaining { b } else { a }),
                (a, b) => a.or(b),
            };
        };
        if let Some(budget) = self.goal.time {
            let f = u32::try_from(elapsed.as_millis() * 1000 / budget.as_millis().max(1)).unwrap_or(1000).min(1000);
            take(f, Some(Eta { remaining: budget.saturating_sub(elapsed), confidence: Confidence::Measured }));
        }
        if let Some(n) = self.goal.passes.map(core::num::NonZeroU64::get) {
            if self.settled >= n {
                take(1000, Some(Eta { remaining: Duration::ZERO, confidence: Confidence::Measured }));
            } else {
                let cycle_done = match (self.cycle, self.cycle_left) {
                    (Some(c), Some(l)) if !c.is_zero() => {
                        1000 - u64::try_from(l.remaining.as_millis() * 1000 / c.as_millis()).unwrap_or(1000).min(1000)
                    },
                    _ => u64::from(pass_percent.min(100)) * 10,
                };
                let f = ((self.settled * 1000 + cycle_done) / n).min(1000);
                let rest = u32::try_from(n - self.settled - 1).unwrap_or(u32::MAX);
                let eta = self.cycle.zip(self.cycle_left).map(|(c, l)| Eta {
                    remaining: l.remaining.saturating_add(c.saturating_mul(rest)),
                    confidence: l.confidence,
                });
                take(u32::try_from(f).unwrap_or(1000), eta.map(rounded));
            }
        }
        Overall { elapsed, fill: fill.map(|f| u16::try_from(f).unwrap_or(1000)), left }
    }
}

/// The run against its goal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Overall {
    pub elapsed: Duration,
    /// How much of the goal is done, per mille; `None` when the goal has no measure (none set, or
    /// only to stop at a fault), in which case a bar would be made up.
    pub fill: Option<u16>,
    /// Time to the goal's end by whichever of its parts comes first.
    pub left: Option<Eta>,
}

/// One observation of the sweep.
pub struct Look {
    pub scope: Scope,
    pub now: Instant,
    /// The pass and test the schedule is at, and the words of work the test has done.
    pub pass: u64,
    pub index: usize,
    pub done: u64,
    /// When the test's wait ends, if the sweep knows.
    pub wait_until: Option<Instant>,
    pub paused: bool,
    /// Time since the run started, for the log.
    pub elapsed: Duration,
}

/// What a test is predicted to take, noted when a prediction first exists and at its middle.
#[derive(Clone, Copy, Debug, Default)]
struct Predicted {
    first: Option<(Duration, Duration)>,
    middle: Option<(Duration, Duration)>,
}

/// The estimator and its surroundings for one memory under test.
struct Lane {
    key: Key,
    names: Vec<&'static str>,
    estimator: Estimator,
    meter: RateMeter,
    pass: u64,
    /// The pass and test last observed, until it is known to have finished.
    seen: Option<(u64, usize)>,
    /// The last test known to have finished. A split dispatch goes on looking at its last round a
    /// while after the test ended, which must not start it over.
    finished: Option<(u64, usize)>,
    last_at: Option<Instant>,
    /// A pause went through a wait, which the clock does not stop for.
    disturbed: bool,
    test: Predicted,
    pass_first: Option<Duration>,
    pass_middle: Option<Duration>,
    /// Time spent in finished tests this pass, for the pass's line in the log.
    spent: Duration,
    forecast: Forecast,
    logged: Option<Instant>,
}

impl Lane {
    fn new(key: Key, schedule: &Schedule, store: &Store) -> Self {
        let names: Vec<&'static str> = TestKind::sequence(schedule.preset()).map(TestKind::name).collect();
        let priors = names.iter().map(|n| store.timings().get(&key, n)).collect();
        Self {
            key,
            names,
            estimator: Estimator::new(schedule.shapes().to_vec(), priors),
            meter: RateMeter::new(),
            pass: schedule.passes(),
            seen: None,
            finished: None,
            last_at: None,
            disturbed: false,
            test: Predicted::default(),
            pass_first: None,
            pass_middle: None,
            spent: Duration::ZERO,
            forecast: Forecast::NONE,
            logged: None,
        }
    }

    fn label(scope: Scope, pass: u64) -> String {
        match scope {
            Scope::Main => format!("pass {pass}"),
            Scope::Reserve => format!("reserve after pass {pass}"),
        }
    }
}

/// Predicted and actual as a signed percentage, for the log.
fn error_pct(predicted: Duration, actual: Duration) -> f64 {
    if actual.is_zero() {
        return 0.0;
    }
    (predicted.as_secs_f64() - actual.as_secs_f64()) * 100.0 / actual.as_secs_f64()
}

/// The sweep's outlook and the memory of what tests took.
pub struct Pace {
    goal: Goal,
    machine: Machine,
    store: Store,
    disk: Option<Box<dyn Disk>>,
    main: Option<Lane>,
    reserve: Option<Lane>,
    /// When the reserve rotation began, while it is under way.
    rotating_since: Option<Instant>,
    /// How long the last reserve rotation took.
    rotation_took: Option<Duration>,
    /// Bytes lent to the firmware per rotation, which is about what the reserve's pass covers, and
    /// the bytes of the sweep's own memory; 0 and 0 with no rotation.
    rotation_scale: (u64, u64),
    /// The pass's predicted length, as of its last observation.
    main_total: Option<Duration>,
    settled: u64,
    last: Outlook,
    rate_at: Option<Instant>,
    write_failed: bool,
}

impl Pace {
    /// A pace with a goal and the machine's timings, saved through `disk`.
    #[must_use]
    pub fn new(goal: Goal, machine: Machine, store: Store, disk: Option<Box<dyn Disk>>) -> Self {
        Self {
            goal,
            machine,
            store,
            disk,
            main: None,
            reserve: None,
            rotating_since: None,
            rotation_took: None,
            rotation_scale: (0, 0),
            main_total: None,
            settled: 0,
            last: Outlook { goal, ..Outlook::NONE },
            rate_at: None,
            write_failed: false,
        }
    }

    /// No goal, no timings on record and nowhere to write them.
    #[must_use]
    pub fn detached() -> Self {
        Self::new(Goal::UNLIMITED, Machine::UNKNOWN, Store::default(), None)
    }

    #[must_use]
    pub const fn goal(&self) -> Goal {
        self.goal
    }

    pub fn set_goal(&mut self, goal: Goal) {
        self.goal = goal;
        self.last.goal = goal;
    }

    /// What the rotation of the reserve covers: the bytes lent for it and the bytes of the sweep's
    /// own memory, which is what scales a pass's time to the rotation's before one has run.
    pub const fn set_rotation_scale(&mut self, lent: u64, main: u64) {
        self.rotation_scale = (lent, main);
    }

    /// Forget the run (not what the tests took), for a restart.
    pub fn restart(&mut self) {
        self.main = None;
        self.reserve = None;
        self.rotating_since = None;
        self.main_total = None;
        self.settled = 0;
        self.last = Outlook { goal: self.goal, ..Outlook::NONE };
        self.rate_at = None;
    }

    /// The outlook as of the last observation.
    #[must_use]
    pub const fn outlook(&self) -> Outlook {
        self.last
    }

    /// Make sure the lane for `scope` is for the setup `schedule` runs, with `cpus` processors and
    /// the widest kernel tier `tier`.
    pub fn prepare(&mut self, scope: Scope, schedule: &Schedule, cpus: usize, tier: &str) {
        let preset: &str = schedule.preset().into();
        let key = Key::new(self.machine.id, preset, schedule.layout().bytes(), cpus, tier);
        let slot = match scope {
            Scope::Main => &mut self.main,
            Scope::Reserve => &mut self.reserve,
        };
        match slot {
            Some(lane) if lane.key == key => {},
            Some(lane) => {
                // The processors or the tier changed mid-run: the tests keep their measured times,
                // and the ones still to run take the times known for the new setup.
                log::info!("timings: setup now {key}, was {}", lane.key);
                let priors = lane.names.iter().map(|n| self.store.timings().get(&key, n)).collect();
                lane.estimator.set_priors(priors);
                lane.key = key;
            },
            None => {
                let lane = Lane::new(key, schedule, &self.store);
                log::debug!("timings: {} of {} tests known for {}", lane.estimator.known(), lane.names.len(), lane.key);
                *slot = Some(lane);
            },
        }
    }

    /// Note where the sweep is. [`Pace::prepare`] must have been called for `look.scope`.
    pub fn observe(&mut self, look: &Look) -> Outlook {
        self.sync_rotation(look);
        let now = look.now;
        let Some(lane) = self.lane_mut(look.scope) else { return self.last };
        let stopped = lane.last_at.map(|at| now - at).filter(|_| look.paused);
        lane.last_at = Some(now);
        if let Some(gap) = stopped {
            // A paused run's clock runs on, but the test's does not; a wait is the memory's own
            // time, which a pause does not stop.
            match lane.forecast.position {
                Position::Work { .. } => lane.estimator.shift(gap),
                Position::Wait { .. } => lane.disturbed = true,
            }
            return self.last;
        }
        if lane.finished == Some((look.pass, look.index)) {
            return self.last;
        }
        // A test or pass other than the last one seen means the last one has ended.
        let mut finished = None;
        if lane.seen != Some((look.pass, look.index)) {
            if let Some((pass, index)) = lane.seen.take() {
                let took = lane.estimator.finish(index, now, Duration::ZERO);
                lane.finished = Some((pass, index));
                finished = Some((pass, index, took, core::mem::take(&mut lane.test)));
            }
            lane.seen = Some((look.pass, look.index));
            lane.meter.reset();
            lane.test = Predicted::default();
        }
        let new_pass = look.pass != lane.pass;
        if let Some((pass, index, took, predicted)) = finished {
            self.finish_test(look.scope, pass, index, took, predicted);
            self.save(now, false);
        }
        if new_pass {
            self.begin_pass(look.scope, look.pass);
        }
        let Some(lane) = self.lane_mut(look.scope) else { return self.last };
        let forecast = lane.estimator.observe(now, look.index, look.done, look.wait_until);
        lane.forecast = forecast;
        match forecast.position {
            Position::Work { .. } => lane.meter.push(now, look.done),
            Position::Wait { .. } => lane.meter.reset(),
        }
        self.note_prediction(look);
        self.refresh(look);
        self.last
    }

    /// Test `index` of pass `pass` ended at `now`: the dispatch of a split test knows when it did.
    /// `fallback` is how long it took by the sweep's own clock.
    pub fn finished(&mut self, scope: Scope, pass: u64, index: usize, now: Instant, fallback: Duration) {
        let Some(lane) = self.lane_mut(scope) else { return };
        if lane.seen == Some((pass, index)) {
            lane.seen = None;
        }
        lane.finished = Some((pass, index));
        let took = lane.estimator.finish(index, now, fallback);
        let predicted = core::mem::take(&mut lane.test);
        self.finish_test(scope, pass, index, took, predicted);
        self.save(now, false);
    }

    fn lane_mut(&mut self, scope: Scope) -> Option<&mut Lane> {
        match scope {
            Scope::Main => self.main.as_mut(),
            Scope::Reserve => self.reserve.as_mut(),
        }
    }

    /// Record what test `index` took, and log how the predictions of it came out.
    fn finish_test(&mut self, scope: Scope, pass: u64, index: usize, took: Duration, predicted: Predicted) {
        let Some(lane) = self.lane_mut(scope) else { return };
        let Some(&name) = lane.names.get(index) else { return };
        let against = |p: Option<(Duration, Duration)>| {
            p.map_or_else(
                || String::from("none"),
                |(at, total)| {
                    format!("{} ({:+.1}%) {} in", Compact(whole(total)), error_pct(total, took), Compact(whole(at)))
                },
            )
        };
        log::info!(
            "eta: {} {name} took {}; predicted first {}, at the middle {}",
            Lane::label(scope, pass),
            Compact(whole(took)),
            against(predicted.first),
            against(predicted.middle)
        );
        lane.spent += took;
        if lane.disturbed {
            lane.disturbed = false;
            return;
        }
        let key = lane.key.clone();
        self.store.record(&key, name, took);
    }

    /// The pass ended: log how its prediction came out and move the estimator on.
    fn begin_pass(&mut self, scope: Scope, pass: u64) {
        let Some(lane) = self.lane_mut(scope) else { return };
        let took = lane.spent;
        let against = |p: Option<Duration>| {
            p.map_or_else(|| String::from("none"), |p| format!("{} ({:+.1}%)", Compact(whole(p)), error_pct(p, took)))
        };
        log::info!(
            "eta: {} took {}; predicted at the start {}, at the middle {}",
            Lane::label(scope, lane.pass),
            Compact(whole(took)),
            against(lane.pass_first),
            against(lane.pass_middle)
        );
        lane.estimator.begin_pass();
        lane.pass = pass;
        lane.spent = Duration::ZERO;
        lane.pass_first = None;
        lane.pass_middle = None;
    }

    fn note_prediction(&mut self, look: &Look) {
        let Some(lane) = self.lane_mut(look.scope) else { return };
        let f = lane.forecast;
        // Only a prediction the screens would show counts: the first moments of an unmeasured test
        // extrapolate from almost nothing.
        if f.shown.is_some() {
            lane.pass_first = lane.pass_first.or(f.pass_total);
            if lane.pass_middle.is_none() && look.index * 2 >= lane.names.len() {
                lane.pass_middle = f.pass_total;
            }
        }
        let Some(left) = f.test_left else { return };
        let point = (f.test_elapsed, f.test_elapsed + left);
        let backed = f.raw.is_some_and(|e| e.confidence == Confidence::Measured);
        if backed || f.test_permille.is_some_and(|p| p >= 100) {
            lane.test.first.get_or_insert(point);
        }
        if f.test_permille.is_some_and(|p| p >= 500) {
            lane.test.middle.get_or_insert(point);
        }
    }

    /// Track the reserve rotation: when it ends, the sweep's own pass resumes with a clock that
    /// ran through it.
    fn sync_rotation(&mut self, look: &Look) {
        match (look.scope, self.rotating_since) {
            (Scope::Reserve, None) => self.rotating_since = Some(look.now),
            (Scope::Main, Some(since)) => {
                let gone = look.now - since;
                self.rotating_since = None;
                self.rotation_took = Some(gone);
                self.reserve = None;
                if let Some(lane) = self.main.as_mut() {
                    lane.estimator.shift(gone);
                    lane.last_at = None;
                }
                log::info!("eta: the reserve rotation took {}", Compact(whole(gone)));
            },
            _ => {},
        }
    }

    /// Rebuild what the screens see from the lane `look` was for.
    fn refresh(&mut self, look: &Look) {
        let rotation = self.rotation_estimate();
        let (scope, now) = (look.scope, look.now);
        let main_total = self.main_total;
        let Some(lane) = (match scope {
            Scope::Main => self.main.as_ref(),
            Scope::Reserve => self.reserve.as_ref(),
        }) else {
            return;
        };
        let f = lane.forecast;
        let mut rate_at = self.rate_at;
        let pass = f.shown.map(rounded);
        let permille = |total: Option<Duration>, left: Option<Duration>| match (total, left) {
            (Some(t), Some(l)) if !t.is_zero() => {
                Some(u16::try_from(1000 - (l.as_millis() * 1000 / t.as_millis()).min(1000)).unwrap_or(0))
            },
            _ => None,
        };
        let pass_permille = permille(f.pass_total, f.raw.map(|e| e.remaining));
        let known = (lane.estimator.known(), lane.names.len());
        let total = f.pass_total;
        let (cycle, cycle_left) = match scope {
            Scope::Main => {
                let cycle = total.zip(rotation.map(|r| r.remaining)).map(|(t, r)| t + r);
                let confidence = |e: Eta| match rotation {
                    Some(r) if r.confidence == Confidence::Estimated => Confidence::Estimated,
                    _ => e.confidence,
                };
                let left = pass
                    .zip(rotation)
                    .map(|(p, r)| rounded(Eta { remaining: p.remaining + r.remaining, confidence: confidence(p) }));
                (cycle.map(whole), left)
            },
            Scope::Reserve => (main_total.zip(total).map(|(m, r)| whole(m + r)), pass),
        };
        let outlook = Outlook {
            goal: self.goal,
            settled: self.settled,
            reserve: scope == Scope::Reserve,
            rate: if matches!(f.position, Position::Wait { .. }) {
                None
            } else if self.rate_at.is_none_or(|at| now - at >= RATE_EVERY) {
                rate_at = Some(now);
                lane.meter.bytes_per_sec().map(|r| r / 1_000_000 * 1_000_000)
            } else {
                self.last.rate
            },
            position: match f.position {
                Position::Wait { nth, waits, left } => Position::Wait { nth, waits, left: whole(left) },
                work @ Position::Work { .. } => work,
            },
            test_left: f.test_left.map(whole),
            test_permille: f.test_permille,
            pass,
            pass_permille,
            cycle,
            cycle_left,
            known,
        };
        if scope == Scope::Main {
            self.main_total = total.or(self.main_total);
        }
        self.rate_at = rate_at;
        self.last = outlook;
        self.audit(scope, now, look.elapsed);
    }

    /// The reserve rotation's time: what the last one took, or this pass's scaled to the reserve's
    /// share of the memory, its waits counted whole, until one has run.
    fn rotation_estimate(&self) -> Option<Eta> {
        let (lent, main) = self.rotation_scale;
        if lent == 0 || main == 0 {
            return Some(Eta { remaining: Duration::ZERO, confidence: Confidence::Measured });
        }
        if let Some(took) = self.rotation_took {
            return Some(Eta { remaining: took, confidence: Confidence::Measured });
        }
        let lane = self.main.as_ref()?;
        let total = lane.forecast.pass_total?;
        let waits: Duration = lane.estimator.shapes().iter().map(ferrite_mem::suite::Shape::waits).sum();
        let work = total.saturating_sub(waits);
        let scaled = work.as_nanos() * u128::from(lent) / u128::from(main);
        let scaled = Duration::from_nanos(u64::try_from(scaled).unwrap_or(u64::MAX));
        Some(Eta { remaining: scaled + waits, confidence: Confidence::Estimated })
    }

    /// Every [`AUDIT_EVERY`]: what is predicted for the pass, the goal and the test.
    fn audit(&mut self, scope: Scope, now: Instant, elapsed: Duration) {
        let outlook = self.last;
        let Some(lane) = (match scope {
            Scope::Main => self.main.as_mut(),
            Scope::Reserve => self.reserve.as_mut(),
        }) else {
            return;
        };
        if lane.logged.is_some_and(|at| now - at < AUDIT_EVERY) {
            return;
        }
        lane.logged = Some(now);
        let show = |e: Option<Eta>| {
            e.map_or_else(
                || String::from("unknown"),
                |e| {
                    let how = if e.confidence == Confidence::Measured { "measured" } else { "estimated" };
                    format!("in {} ({how})", Compact(e.remaining))
                },
            )
        };
        let test = lane.names.get(lane.seen.map_or(0, |(_, i)| i)).copied().unwrap_or("none");
        let goal = outlook.overall(elapsed, 0).left;
        let goal_text = if outlook.goal.is_unlimited() {
            String::from("no goal")
        } else {
            format!("goal ({}) ends {}", outlook.goal, show(goal))
        };
        log::info!(
            "eta: {} ends {}, {goal_text}, {test} ends {}",
            Lane::label(scope, lane.pass),
            show(outlook.pass),
            outlook.test_left.map_or_else(|| String::from("unknown"), |t| format!("in {}", Compact(t)))
        );
    }

    /// Write the timings out if there is a change to write and it is due, or `force`.
    pub fn save(&mut self, now: Instant, force: bool) {
        let Some(disk) = self.disk.as_mut() else { return };
        let Some(text) = self.store.take_save(now, force) else { return };
        match disk.write(text.as_bytes()) {
            Ok(()) => self.write_failed = false,
            Err(e) => {
                self.store.unsaved();
                if !self.write_failed {
                    log::warn!("timings: writing {} failed: {e}", crate::timings::PATH);
                }
                self.write_failed = true;
            },
        }
    }

    /// Passes done together with their reserve rotation, as the sweep counts them.
    pub const fn set_settled(&mut self, settled: u64) {
        self.settled = settled;
        self.last.settled = settled;
    }
}

#[cfg(test)]
mod tests {
    use alloc::rc::Rc;
    use core::cell::RefCell;
    use core::num::NonZeroU64;

    use assert2::{assert, check};
    use ferrite_mem::suite::{Config, Kernels, Preset};
    use ferrite_mem::{Kernel, Layout};
    use rstest::rstest;

    use super::*;
    use crate::timings::Timings;

    const CFG: Config = Config {
        fixed_iterations: 1,
        byte_iterations: 1,
        random_rounds: 4,
        modulo_rounds: 1,
        move_iterations: 2,
        bus_iterations: 2,
        fade: Duration::from_secs(5),
        address_window_bytes: 4 << 10,
        move_window_bytes: 4 << 10,
        ..Config::DEFAULT
    };

    const WORDS: usize = 2048;

    fn schedule(preset: Preset) -> Schedule {
        Schedule::new(Layout::new([WORDS]), Kernels::uniform(Kernel::SCALAR), CFG, preset)
    }

    fn at(secs: u64) -> Instant {
        Instant::from_epoch(Duration::from_secs(secs))
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    fn look(scope: Scope, now: u64, pass: u64, index: usize, done: u64) -> Look {
        Look { scope, now: at(now), pass, index, done, wait_until: None, paused: false, elapsed: secs(now) }
    }

    /// A disk that keeps every file written to it.
    #[derive(Clone, Default)]
    struct Memory(Rc<RefCell<Vec<Vec<u8>>>>);

    impl Disk for Memory {
        fn write(&mut self, bytes: &[u8]) -> Result<(), uefi::Error> {
            self.0.borrow_mut().push(bytes.to_vec());
            Ok(())
        }
    }

    fn key(preset: Preset) -> Key {
        Key::new(Machine::UNKNOWN.id, preset.into(), WORDS * 8, 1, "scalar")
    }

    fn pace_with(priors: &[(Preset, u64)], disk: Option<Memory>) -> Pace {
        let mut timings = Timings::default();
        for &(preset, took) in priors {
            for kind in TestKind::sequence(preset) {
                timings.record(&key(preset), kind.name(), secs(took));
            }
        }
        Pace::new(Goal::UNLIMITED, Machine::UNKNOWN, Store::new(timings), disk.map(|d| Box::new(d) as Box<dyn Disk>))
    }

    #[test]
    fn what_the_tests_took_before_makes_the_first_second_measured() {
        let schedule = schedule(Preset::Standard);
        let tests = schedule.shapes().len() as u64;
        let mut pace = pace_with(&[(Preset::Standard, 10)], None);
        pace.prepare(Scope::Main, &schedule, 1, "scalar");
        let outlook = pace.observe(&look(Scope::Main, 0, 0, 0, 0));
        check!(outlook.pass == Some(Eta { remaining: secs(10 * tests), confidence: Confidence::Measured }));
        check!(outlook.known == (tests as usize, tests as usize));
        // A different preset, or another number of processors, is another setup: nothing known.
        let mut other = pace_with(&[(Preset::Standard, 10)], None);
        other.prepare(Scope::Main, &schedule, 4, "scalar");
        let outlook = other.observe(&look(Scope::Main, 0, 0, 0, 0));
        check!(outlook.pass.is_none() && outlook.known == (0, tests as usize));
    }

    #[test]
    fn a_finished_test_is_kept_under_its_setup_and_written_at_most_every_ten_seconds() {
        let schedule = schedule(Preset::Standard);
        let disk = Memory::default();
        let mut pace = pace_with(&[], Some(disk.clone()));
        pace.prepare(Scope::Main, &schedule, 1, "scalar");
        pace.observe(&look(Scope::Main, 0, 0, 0, 0));
        // The next test showing up is the first one's end, three seconds in.
        pace.observe(&look(Scope::Main, 3, 0, 1, 0));
        let writes = disk.0.borrow().clone();
        assert!(let [file] = writes.as_slice());
        assert!(let Ok(timings) = Timings::parse(file));
        let first = TestKind::sequence(Preset::Standard).next().unwrap().name();
        check!(timings.get(&key(Preset::Standard), first) == Some(secs(3)));
        // Four seconds later another ends; too soon to write again. The end of the run writes it.
        pace.observe(&look(Scope::Main, 7, 0, 2, 0));
        check!(disk.0.borrow().len() == 1);
        pace.save(at(8), true);
        assert!(let Some(last) = disk.0.borrow().last().cloned());
        assert!(let Ok(timings) = Timings::parse(&last));
        check!(timings.len() == 2);
    }

    #[test]
    fn a_dispatch_that_knows_when_its_test_ended_says_so() {
        let schedule = schedule(Preset::Standard);
        let mut pace = pace_with(&[], Some(Memory::default()));
        pace.prepare(Scope::Main, &schedule, 8, "avx2");
        pace.observe(&look(Scope::Main, 0, 0, 0, 0));
        pace.finished(Scope::Main, 0, 0, at(4), secs(99));
        // The dispatch looks at its last round a while longer; that is not the test starting over.
        pace.observe(&look(Scope::Main, 5, 0, 0, 100));
        pace.observe(&look(Scope::Main, 6, 0, 1, 0));
        let first = TestKind::sequence(Preset::Standard).next().unwrap().name();
        let key = Key::new(Machine::UNKNOWN.id, "standard", WORDS * 8, 8, "avx2");
        check!(pace.store.timings().get(&key, first) == Some(secs(4)), "by the estimator's clock, not the fallback");
        check!(pace.store.timings().len() == 1);
    }

    #[test]
    fn a_pause_is_not_part_of_the_test_it_interrupts() {
        let schedule = schedule(Preset::Standard);
        let mut pace = pace_with(&[], None);
        pace.prepare(Scope::Main, &schedule, 1, "scalar");
        let mut work = |now, done, paused| {
            let mut l = look(Scope::Main, now, 0, 0, done);
            l.paused = paused;
            pace.observe(&l)
        };
        for t in 0..=5 {
            work(t, t * 10, false);
        }
        for t in 6..=65 {
            work(t, 50, true);
        }
        for t in 66..=70 {
            work(t, (t - 60) * 5 + 25, false);
        }
        pace.finished(Scope::Main, 0, 0, at(71), secs(71));
        let first = TestKind::sequence(Preset::Standard).next().unwrap().name();
        let took = pace.store.timings().get(&key(Preset::Standard), first).unwrap();
        check!(took >= secs(10) && took <= secs(13), "ten seconds of work, a minute paused: {took:?}");
    }

    #[test]
    fn the_rotation_of_the_reserve_does_not_age_the_test_the_pass_resumes_with() {
        let main = schedule(Preset::Standard);
        let reserve = Schedule::new(Layout::new([WORDS / 4]), Kernels::uniform(Kernel::SCALAR), CFG, Preset::Standard);
        let mut pace = pace_with(&[(Preset::Standard, 10)], None);
        pace.set_rotation_scale(WORDS as u64 * 2, WORDS as u64 * 8);
        pace.prepare(Scope::Main, &main, 1, "scalar");
        pace.observe(&look(Scope::Main, 0, 0, 0, 0));
        pace.prepare(Scope::Reserve, &reserve, 1, "scalar");
        let during = pace.observe(&look(Scope::Reserve, 5, 0, 0, 0));
        check!(during.reserve);
        // 600 s later the pass carries on with its first test, which has had a few seconds.
        pace.prepare(Scope::Main, &main, 1, "scalar");
        let after = pace.observe(&look(Scope::Main, 605, 0, 0, 0));
        check!(!after.reserve);
        check!(after.pass.map(|e| e.remaining) == Some(secs(10 * main.shapes().len() as u64 - 5)), "{:?}", after.pass);
        check!(pace.rotation_took == Some(secs(600)));
    }

    #[test]
    fn looking_at_a_test_in_progress_allocates_nothing() {
        let schedule = schedule(Preset::Thorough);
        let mut pace = pace_with(&[(Preset::Thorough, 10)], None);
        pace.prepare(Scope::Main, &schedule, 1, "scalar");
        let mut at_ms = 0;
        // Long enough to fill the ring of predictions and pass the first audit line.
        while at_ms < 15_000 {
            let mut l = look(Scope::Main, 0, 0, 0, at_ms / 10);
            l.now = Instant::from_epoch(Duration::from_millis(at_ms));
            pace.observe(&l);
            at_ms += 200;
        }
        let mut l = look(Scope::Main, 0, 0, 0, at_ms / 10);
        l.now = Instant::from_epoch(Duration::from_millis(at_ms));
        let (_, allocations) = crate::testing::count(|| pace.observe(&l));
        check!(allocations == 0);
    }

    fn outlook(goal: Goal) -> Outlook {
        Outlook {
            goal,
            settled: 1,
            cycle: Some(secs(2400)),
            cycle_left: Some(Eta { remaining: secs(1500), confidence: Confidence::Estimated }),
            ..Outlook::NONE
        }
    }

    fn goal(passes: Option<u64>, time: Option<u64>) -> Goal {
        Goal { passes: passes.and_then(NonZeroU64::new), time: time.map(secs), stop_on_fault: false }
    }

    #[rstest]
    #[case::no_goal_no_bar(goal(None, None), 1800, None, None)]
    #[case::time_budget(goal(None, Some(7200)), 1800, Some(250), Some((5400, Confidence::Measured)))]
    #[case::passes_by_the_cycle(goal(Some(3), None), 1800, Some(458), Some((3900, Confidence::Estimated)))]
    #[case::the_nearer_end_wins(goal(Some(3), Some(3600)), 1800, Some(500), Some((1800, Confidence::Measured)))]
    #[case::past_its_budget(goal(None, Some(600)), 1800, Some(1000), Some((0, Confidence::Measured)))]
    fn the_overall_bar_measures_the_run_against_its_goal(
        #[case] goal: Goal,
        #[case] elapsed: u64,
        #[case] fill: Option<u16>,
        #[case] left: Option<(u64, Confidence)>,
    ) {
        let overall = outlook(goal).overall(secs(elapsed), 0);
        check!(overall.fill == fill);
        check!(overall.left.map(|e| (e.remaining.as_secs(), e.confidence)) == left);
    }

    #[test]
    fn a_goal_of_passes_falls_back_on_words_until_there_is_a_forecast_and_ends_when_met() {
        let mut o = Outlook { goal: goal(Some(4), None), settled: 1, ..Outlook::NONE };
        let overall = o.overall(secs(100), 50);
        check!(overall.fill == Some(375) && overall.left.is_none(), "(1 pass + half of one) of 4");
        o.settled = 4;
        check!(o.overall(secs(100), 0).fill == Some(1000));
        let only_fault = Outlook { goal: Goal { stop_on_fault: true, ..Goal::UNLIMITED }, ..Outlook::NONE };
        check!(only_fault.overall(secs(5), 90).fill.is_none(), "stopping at a fault has nothing to fill a bar with");
    }
}
