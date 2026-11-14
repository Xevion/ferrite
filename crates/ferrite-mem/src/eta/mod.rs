//! How long a pass has left, as time: from the shape of its tests, what they took before, and what
//! they have taken so far.
//!
//! A count of words says little about time. Bit fade is a fifth of a pass's time and almost none
//! of its words, and tests differ several-fold in time per word. What does hold is that every
//! test but bit fade runs at a steady speed from start to end, and that a test takes as long on the
//! next pass as on this one. The [`Estimator`] uses both:
//!
//! - a finished test counts for the time it took;
//! - the running test is a blend of its previous time (the prior) and a straight-line extrapolation
//!   of its own progress, trusting the extrapolation more the further it is (all of it from a
//!   quarter of the test on), with its waits taken exactly;
//! - a test still to run counts for its prior, or, with none, for its words at the speed this pass
//!   has measured so far (once that is half a second of work or more, else at the calibrated speed),
//!   plus its waits exactly.
//!
//! A prediction is only [`Measured`](Confidence::Measured) when every test left has a prior and the
//! running test is not running at a speed that contradicts its own. Otherwise it is
//! [`Estimated`](Confidence::Estimated), and is withheld until successive predictions agree.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::time::Duration;

use ferrite_hal::Instant;

use crate::suite::{Place, Shape, Stage};

mod basis;
mod rate;
#[cfg(test)]
mod tests;

pub use basis::BasisPoints;
pub use rate::RateMeter;

/// Per mille of a test done at which its own extrapolation has all the weight, and from which it
/// is held against the prior.
const LIVE_FROM: u128 = 250;
/// How far (percent of the prior) the extrapolation may stray from the prior before the prediction
/// stops being [`Confidence::Measured`], and the least slack (for short tests) in time.
const AGREE_PERCENT: u32 = 10;
const AGREE_SLACK: Duration = Duration::from_secs(2);
/// Predictions are compared once a second.
const SAMPLE_EVERY: Duration = Duration::from_secs(1);
/// How far back the predictions that must agree reach, and the least they must span.
const GATE_KEEP: Duration = Duration::from_secs(10);
const GATE_SPAN: Duration = Duration::from_secs(5);
/// How far (percent of the time left, at least [`GATE_SLACK`]) a prediction of the end may be from
/// the mean of the recent ones.
const GATE_PERCENT: u32 = 2;
const GATE_SLACK: Duration = Duration::from_secs(1);
/// The least time of work a pace or a straight line is taken from. A test that took milliseconds
/// over a few words says nothing about a test of a billion, and scaled up it gives days.
const MIN_SAMPLE: Duration = Duration::from_millis(500);

/// What a prediction rests on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confidence {
    /// Extrapolated from this pass alone, or contradicted by it: show it as approximate.
    Estimated,
    /// Backed by what every remaining test took before.
    Measured,
}

/// Time left, and how far to trust it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Eta {
    pub remaining: Duration,
    pub confidence: Confidence,
}

/// Where the running test is, for display.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Position {
    /// Touching memory; `waits_passed` of the test's `waits` are behind it.
    Work { waits_passed: usize, waits: usize },
    /// In its `nth` wait of `waits`, with `left` of it to go.
    Wait { nth: usize, waits: usize, left: Duration },
}

impl Position {
    pub const NONE: Self = Self::Work { waits_passed: 0, waits: 0 };
}

/// What the estimator makes of the pass at one moment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Forecast {
    /// Time left in the pass, once there is reason to show it: backed by priors, or the recent
    /// predictions agree and some work is done.
    pub shown: Option<Eta>,
    /// Time left as predicted now, whether or not it is fit to show.
    pub raw: Option<Eta>,
    /// Time left in the running test.
    pub test_left: Option<Duration>,
    /// How far through the running test it is: by words for a test that only works, by time for
    /// one that waits, whose words stand still for minutes at a time.
    pub test_progress: Option<BasisPoints>,
    /// How far through the running test's words it is, whatever it waits for.
    pub test_words: BasisPoints,
    /// How long the running test has been going, waits included, and how much of that was work.
    pub test_elapsed: Duration,
    pub test_work: Duration,
    /// The pass's whole length as predicted now: what has passed plus what is left.
    pub pass_total: Option<Duration>,
    /// How far through the pass it is by time, once it has a length.
    pub pass_progress: Option<BasisPoints>,
    /// How far through the pass's words it is.
    pub pass_words: BasisPoints,
    pub position: Position,
}

impl Forecast {
    pub const NONE: Self = Self {
        shown: None,
        raw: None,
        test_left: None,
        test_progress: None,
        test_words: BasisPoints::ZERO,
        test_elapsed: Duration::ZERO,
        test_work: Duration::ZERO,
        pass_total: None,
        pass_progress: None,
        pass_words: BasisPoints::ZERO,
        position: Position::NONE,
    };
}

/// The test being timed.
#[derive(Clone, Copy, Debug)]
struct Current {
    index: usize,
    started: Instant,
    last_done: u64,
    /// When the wait at stage `.0` began.
    wait: Option<(usize, Instant)>,
}

/// Predicts the time left in a pass of tests with the given shapes.
///
/// Feed it with [`Estimator::observe`] as often as it is convenient and [`Estimator::finish`] at
/// each test's end; nothing here allocates after construction except the ring of recent predictions.
#[derive(Clone, Debug)]
pub struct Estimator {
    shapes: Vec<Shape>,
    /// What each test took on an earlier pass, if known.
    priors: Vec<Option<Duration>>,
    /// What each test took on this one.
    took: Vec<Option<Duration>>,
    /// Tests of this pass that ended before anything looked at them, which count as taking no time.
    unwatched: Vec<bool>,
    current: Option<Current>,
    /// When the next test starts, if the last one has just finished.
    next_start: Option<Instant>,
    /// The end of the pass as predicted at each sample: (when sampled, predicted end).
    samples: VecDeque<(Instant, Instant)>,
    forecast: Forecast,
    /// Bytes of work per second a calibration measured, to guess with before any test has finished.
    speed: Option<u64>,
}

/// `d * num / den`, in nanoseconds so nothing overflows or truncates early.
fn scale(d: Duration, num: u128, den: u128) -> Duration {
    if den == 0 {
        return Duration::ZERO;
    }
    let nanos = d.as_nanos() * num / den;
    Duration::new(
        u64::try_from(nanos / 1_000_000_000).unwrap_or(u64::MAX),
        u32::try_from(nanos % 1_000_000_000).unwrap_or(0),
    )
}

/// The place of the wait at stage `stage` of `shape`, if that stage is one.
fn wait_at(shape: &Shape, stage: usize) -> Option<Place> {
    matches!(shape.stages().get(stage), Some(Stage::Wait(_))).then(|| Place::Wait {
        stage,
        nth: shape.stages()[..=stage].iter().filter(|s| matches!(s, Stage::Wait(_))).count(),
        waits: shape.wait_count(),
    })
}

/// The place of the wait right after the work stage `stage`, if it is followed by one.
fn wait_after(shape: &Shape, stage: usize) -> Option<Place> {
    wait_at(shape, stage + 1)
}

fn nanos(at: Instant) -> i128 {
    at.since_epoch().as_nanos().cast_signed()
}

impl Estimator {
    /// An estimator for a pass of `shapes`, with `priors` (one per shape, `None` for a test never
    /// timed).
    #[must_use]
    pub fn new(shapes: Vec<Shape>, mut priors: Vec<Option<Duration>>) -> Self {
        priors.resize(shapes.len(), None);
        let took = alloc::vec![None; shapes.len()];
        let shapes_len = shapes.len();
        Self {
            shapes,
            priors,
            took,
            unwatched: alloc::vec![false; shapes_len],
            current: None,
            next_start: None,
            samples: VecDeque::new(),
            forecast: Forecast::NONE,
            speed: None,
        }
    }

    /// Guess the time of a test with no prior at `bytes_per_sec` of work until this pass has
    /// measured a pace of its own.
    #[must_use]
    pub const fn with_speed(mut self, bytes_per_sec: Option<u64>) -> Self {
        self.speed = bytes_per_sec;
        self
    }

    #[must_use]
    pub fn shapes(&self) -> &[Shape] {
        &self.shapes
    }

    /// The latest forecast, as of the last [`Estimator::observe`].
    #[must_use]
    pub const fn forecast(&self) -> Forecast {
        self.forecast
    }

    /// How many tests have a prior.
    #[must_use]
    pub fn known(&self) -> usize {
        self.priors.iter().flatten().count()
    }

    /// Replace the priors, for a pass whose machine setup changed under it.
    pub fn set_priors(&mut self, mut priors: Vec<Option<Duration>>) {
        priors.resize(self.shapes.len(), None);
        self.priors = priors;
    }

    /// Take the shapes of the tests as they are now, for memory that came back from a rotation of a
    /// different size: where a wait begins is a count of words, so shapes of another layout place
    /// it wrong. Nothing is done, or allocated, when they are the same; they must be one per test
    /// still, so shapes of another preset are left alone.
    pub fn reshape(&mut self, shapes: &[Shape]) {
        if self.shapes.len() == shapes.len() && self.shapes != shapes {
            self.shapes = shapes.to_vec();
        }
    }

    /// Start the next pass: what each test took now stands in for what it took before.
    pub fn begin_pass(&mut self) {
        for (prior, took) in self.priors.iter_mut().zip(&mut self.took) {
            *prior = took.take().or(*prior);
        }
        self.unwatched.fill(false);
        self.current = None;
    }

    /// Note that the pass is first looked at in test `index`: the tests before it that have no time
    /// of this pass ended between two looks, and count for none, so the pass still has a length.
    pub fn skip_to(&mut self, index: usize) {
        for (unwatched, took) in self.unwatched.iter_mut().zip(&self.took).take(index) {
            *unwatched |= took.is_none();
        }
    }

    /// Put every instant the estimator holds `by` later, after the clock ran while the test did not.
    pub fn shift(&mut self, by: Duration) {
        if let Some(c) = self.current.as_mut() {
            c.started = c.started + by;
            c.wait = c.wait.map(|(stage, at)| (stage, at + by));
        }
        self.next_start = self.next_start.map(|at| at + by);
        for (sampled, end) in &mut self.samples {
            *sampled = *sampled + by;
            *end = *end + by;
        }
    }

    /// Note that test `index` finished at `now`, and return how long it took: by this estimator's
    /// own clock when it was watching the test, else `fallback`.
    pub fn finish(&mut self, index: usize, now: Instant, fallback: Duration) -> Duration {
        let took = match self.current {
            Some(c) if c.index == index => now - c.started,
            _ => fallback,
        };
        if let Some(slot) = self.took.get_mut(index) {
            *slot = Some(took);
        }
        self.current = None;
        self.next_start = Some(now);
        took
    }

    /// Where the running test, number `index` of the pass, is at `now`, having done `done` words of
    /// work. `wait_until` is when its wait ends, if the caller knows; without it a wait is taken to
    /// have begun the first time it is seen.
    pub fn observe(&mut self, now: Instant, index: usize, done: u64, wait_until: Option<Instant>) -> Forecast {
        let Some(shape) = self.shapes.get(index) else {
            self.forecast = Forecast::NONE;
            return self.forecast;
        };
        let restarted = self.current.is_none_or(|c| c.index != index || done < c.last_done);
        if restarted {
            let started = self.next_start.take().unwrap_or(now);
            self.current = Some(Current { index, started, last_done: done, wait: None });
        }
        let mut place = shape.locate(done);
        let mut current = self.current.unwrap_or(Current { index, started: now, last_done: done, wait: None });
        current.last_done = done;
        // A caller that knows the memory is sitting out a wait is believed over the word count,
        // which only places a wait when it lands on the stage's boundary exactly.
        if wait_until.is_some_and(|end| now < end)
            && let Place::Work { stage, .. } = place
            && let Some(waiting) =
                wait_after(shape, stage).or_else(|| current.wait.and_then(|(s, _)| wait_at(shape, s)))
        {
            place = waiting;
        }
        let mut position = Position::Work { waits_passed: 0, waits: shape.wait_count() };
        let mut wait_elapsed = Duration::ZERO;
        let mut stage_now = shape.stages().len();
        match place {
            Place::Wait { stage, nth, waits } => {
                let length = match shape.stages()[stage] {
                    Stage::Wait(d) => d,
                    Stage::Work(_) => Duration::ZERO,
                };
                let began = match current.wait {
                    Some((s, at)) if s == stage => at,
                    _ => wait_until.map_or(now, |end| Instant::from_epoch(end.since_epoch().saturating_sub(length))),
                };
                current.wait = Some((stage, began));
                wait_elapsed = (now - began).min(length);
                stage_now = stage;
                position = Position::Wait { nth, waits, left: length.saturating_sub(wait_elapsed) };
            },
            Place::Work { stage, .. } => {
                stage_now = stage;
                let waits_passed = shape.stages()[..stage].iter().filter(|s| matches!(s, Stage::Wait(_))).count();
                position = Position::Work { waits_passed, waits: shape.wait_count() };
            },
            Place::Finished => {},
        }
        self.current = Some(current);

        let waited_before: Duration = shape.stages()[..stage_now.min(shape.stages().len())]
            .iter()
            .map(|s| if let Stage::Wait(d) = s { *d } else { Duration::ZERO })
            .sum();
        let waited = waited_before + wait_elapsed;
        let elapsed = now - current.started;
        let work_time = elapsed.saturating_sub(waited);
        let waits_left = shape.waits().saturating_sub(waited);

        let forecast = self.predict(index, done, work_time, waits_left, elapsed, position);
        let shown = self.sample(now, &forecast, done);
        self.forecast = Forecast { shown, ..forecast };
        self.forecast
    }

    /// Time per word of work over the tests this pass has finished, as (time, words).
    fn finished_pace(&self) -> (Duration, u128) {
        let mut time = Duration::ZERO;
        let mut words = 0_u128;
        for (shape, took) in self.shapes.iter().zip(&self.took) {
            if let Some(took) = took {
                time += took.saturating_sub(shape.waits());
                words += u128::from(shape.work());
            }
        }
        (time, words)
    }

    fn predict(
        &self,
        index: usize,
        done: u64,
        work_time: Duration,
        waits_left: Duration,
        elapsed: Duration,
        position: Position,
    ) -> Forecast {
        let shape = &self.shapes[index];
        let work = shape.work();
        let done = done.min(work);
        let (fin_time, fin_words) = self.finished_pace();

        // The running test's work time: prior or this pass's pace, blended with its own line.
        let prior_work = self.priors[index].map(|p| p.saturating_sub(shape.waits()));
        let live = (done > 0 && work_time >= MIN_SAMPLE && work > 0)
            .then(|| scale(work_time, u128::from(work), u128::from(done)));
        let pace_guess = self.guess(fin_time, fin_words, work);
        let base = prior_work.or(pace_guess);
        let frac = if work == 0 { 1000 } else { u128::from(done) * 1000 / u128::from(work) };
        let weight = (frac * 1000 / LIVE_FROM).min(1000);
        let work_total = match (base, live) {
            (Some(b), Some(l)) => Some(scale(b, 1000 - weight, 1000) + scale(l, weight, 1000)),
            (Some(b), None) => Some(b),
            (None, Some(l)) => Some(l),
            (None, None) => None,
        };
        let finished_test = done >= work;
        let work_left =
            if finished_test { Some(Duration::ZERO) } else { work_total.map(|t| t.saturating_sub(work_time)) };
        let test_left = work_left.map(|w| w + waits_left);

        // The tests after it: priors, else the pass's pace so far (this test's part included).
        let pace_time = fin_time + work_time;
        let pace_words = fin_words + u128::from(done);
        let mut later = Some(Duration::ZERO);
        let mut later_all_known = true;
        for (k, shape_k) in self.shapes.iter().enumerate().skip(index + 1) {
            later_all_known &= self.priors[k].is_some();
            let guess = self.priors[k]
                .or_else(|| self.guess(pace_time, pace_words, shape_k.work()).map(|work| work + shape_k.waits()));
            later = later.zip(guess).map(|(a, b)| a + b);
        }

        let remaining = test_left.zip(later).map(|(a, b)| a + b);
        let agrees = match (prior_work, live) {
            (Some(p), Some(l)) if frac >= LIVE_FROM => l.abs_diff(p) <= (p * AGREE_PERCENT / 100).max(AGREE_SLACK),
            _ => true,
        };
        let backed = self.priors[index].is_some() && later_all_known && agrees;
        let confidence = if backed { Confidence::Measured } else { Confidence::Estimated };
        let raw = remaining.map(|remaining| Eta { remaining, confidence });

        let before: Option<Duration> = (0..index)
            .map(|k| self.took[k].or(self.priors[k]).or_else(|| self.unwatched[k].then_some(Duration::ZERO)))
            .sum();
        let pass_total = before.zip(remaining).map(|(b, r)| b + elapsed + r);
        let pass_progress = before.zip(pass_total).map(|(b, total)| BasisPoints::of_time(b + elapsed, total));
        let test_words = BasisPoints::of(done, work);
        let test_progress = if shape.wait_count() == 0 {
            Some(test_words)
        } else {
            test_left.map(|left| BasisPoints::of_time(elapsed, elapsed + left))
        };
        let pass_work: u64 = self.shapes.iter().map(Shape::work).sum();
        let words_before: u64 = self.shapes[..index].iter().map(Shape::work).sum();
        Forecast {
            shown: None,
            raw,
            test_left,
            test_progress,
            test_words,
            test_elapsed: elapsed,
            test_work: work_time,
            pass_total,
            pass_progress,
            pass_words: BasisPoints::of(words_before + done, pass_work),
            position,
        }
    }

    /// The time `words` of work take at the pace of `time` for `done` words, or at the calibrated
    /// speed while nothing long enough has been measured.
    fn guess(&self, time: Duration, done: u128, words: u64) -> Option<Duration> {
        if done > 0 && time >= MIN_SAMPLE {
            return Some(scale(time, u128::from(words), done));
        }
        let bytes_per_sec = u128::from(self.speed.filter(|&s| s > 0)?);
        Some(scale(Duration::from_secs(1), u128::from(words) * size_of::<u64>() as u128, bytes_per_sec))
    }

    /// Record this forecast's end once a second, and give it back if it is fit to show.
    fn sample(&mut self, now: Instant, forecast: &Forecast, done: u64) -> Option<Eta> {
        let Some(raw) = forecast.raw else {
            self.samples.clear();
            return None;
        };
        if self.samples.back().is_none_or(|&(at, _)| now - at >= SAMPLE_EVERY) {
            self.samples.push_back((now, now + raw.remaining));
        }
        while self.samples.front().is_some_and(|&(at, _)| now - at > GATE_KEEP) {
            self.samples.pop_front();
        }
        let worked = done > 0 || self.took.iter().any(Option::is_some);
        let fit = raw.confidence == Confidence::Measured || (worked && self.agree(raw.remaining));
        fit.then_some(raw)
    }

    /// Whether the recent predictions of the end agree to within the band, over at least [`GATE_SPAN`].
    fn agree(&self, remaining: Duration) -> bool {
        let (Some(&(first, _)), Some(&(last, _))) = (self.samples.front(), self.samples.back()) else {
            return false;
        };
        if last - first < GATE_SPAN {
            return false;
        }
        let n = self.samples.len() as i128;
        let mean = self.samples.iter().map(|&(_, end)| nanos(end)).sum::<i128>() / n;
        let band = (remaining * GATE_PERCENT / 100).max(GATE_SLACK).as_nanos().cast_signed();
        self.samples.iter().all(|&(_, end)| (nanos(end) - mean).abs() <= band)
    }
}
