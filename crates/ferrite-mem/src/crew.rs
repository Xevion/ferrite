//! Consecutive tests of the memory suite on several cores at once, each core on its own share of
//! the memory throughout.
//!
//! The BSP gives each core a [`Job`]: the core's [`Test`] state machine for each test of the run
//! and the regions of its share, so no test state lives on a core's stack. An AP runs its job once
//! for the whole run ([`Job::run_ap`]); the BSP works its own job a turn at a time between its
//! other duties. The cores meet at the start, at every gated phase boundary ([`Test::gated`]), where
//! each waits until all have ended the phase, and between tests, where each waits for the BSP to
//! begin the next round ([`Crew::begin_round`]) or close the crew ([`Crew::close`]), so an AP is
//! dispatched once rather than once per test. The BSP opens the gates ([`Crew::coordinate`]). Each
//! core reports through its [`Seat`], which holds only atomics.

use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use core::time::Duration;

use ferrite_hal::{Clock, Instant};

use crate::suite::{Kernels, Progress, Test};
use crate::{CpuAddr, Fault, Faults, Regions, Tier};

/// The first faults each core keeps per test with their addresses; the rest are only counted.
pub const SEAT_FAULTS: usize = 16;
/// How long an AP waits at the start for the BSP's release before giving up.
pub const AP_PATIENCE: Duration = Duration::from_secs(5);

/// Protection for work in the wide vector registers, where the platform needs it.
pub trait WideGuard {
    /// Run `f` under the platform's protection for wide-register work.
    fn wide(&self, f: &mut dyn FnMut());
}

/// What a core says about itself before it works an AP job.
pub trait Platform {
    type Wide: WideGuard;

    /// Whether SSE instructions run on this core.
    fn sse_ready(&self) -> bool;

    /// Turn the wide vector state on for the job, `None` when it cannot be.
    fn enable_wide(&self) -> Option<Self::Wide>;

    /// Whether kernels of `tier` run on this core as it is now.
    fn tier_usable(&self, tier: Tier) -> bool;
}

/// Why a core did not run its share.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The firmware left SSE off on this AP.
    NoSse,
    /// A kernel's tier does not run on this core.
    Tier,
    /// The BSP's release did not come in time.
    StartLine,
    /// The job was already taken by another core.
    Taken,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeatState {
    /// Not run yet; an AP still here after its dispatch ended was reset before it began.
    Idle,
    /// At the start, waiting for the release.
    Ready,
    Running,
    Done,
    /// Ended early because the crew was stopped.
    Stopped,
    Failed(Failure),
    Unknown(u8),
}

impl SeatState {
    const fn raw(self) -> u8 {
        match self {
            Self::Idle => 0,
            Self::Ready => 1,
            Self::Running => 2,
            Self::Done => 3,
            Self::Stopped => 4,
            Self::Failed(Failure::NoSse) => 0x10,
            Self::Failed(Failure::Tier) => 0x11,
            Self::Failed(Failure::StartLine) => 0x12,
            Self::Failed(Failure::Taken) => 0x13,
            Self::Unknown(raw) => raw,
        }
    }

    const fn from_raw(raw: u8) -> Self {
        match raw {
            0 => Self::Idle,
            1 => Self::Ready,
            2 => Self::Running,
            3 => Self::Done,
            4 => Self::Stopped,
            0x10 => Self::Failed(Failure::NoSse),
            0x11 => Self::Failed(Failure::Tier),
            0x12 => Self::Failed(Failure::StartLine),
            0x13 => Self::Failed(Failure::Taken),
            raw => Self::Unknown(raw),
        }
    }

    /// Whether the core has left its job for good.
    #[must_use]
    pub const fn ended(self) -> bool {
        matches!(self, Self::Done | Self::Stopped | Self::Failed(_))
    }
}

impl fmt::Display for SeatState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Idle => f.write_str("never ran (reset or not started)"),
            Self::Ready => f.write_str("stopped at the start (timed out or panicked)"),
            Self::Running => f.write_str("stopped mid-test (timed out or panicked)"),
            Self::Done => f.write_str("done"),
            Self::Stopped => f.write_str("stopped"),
            Self::Failed(Failure::NoSse) => f.write_str("failed: SSE disabled"),
            Self::Failed(Failure::Tier) => f.write_str("failed: kernel tier unusable on this core"),
            Self::Failed(Failure::StartLine) => f.write_str("failed: no release at the start"),
            Self::Failed(Failure::Taken) => f.write_str("failed: job already taken"),
            Self::Unknown(raw) => write!(f, "unknown state {raw:#x}"),
        }
    }
}

#[derive(Default)]
struct FaultCell {
    addr: AtomicUsize,
    expected: AtomicU64,
    actual: AtomicU64,
}

/// One core's progress and results for one test, written only by that core. A cache line of its
/// own, so no two cores share one.
#[derive(Default)]
#[repr(align(64))]
pub struct Seat {
    state: AtomicU8,
    /// The phase this core waits at a gate to start; at or below the crew's open gate when it
    /// does not wait.
    gate: AtomicUsize,
    /// The last round whose test this core has finished.
    finished: AtomicUsize,
    /// Work done in the current test, in its units.
    done: AtomicU64,
    faults: AtomicU64,
    /// Cells of `first` filled; a filled cell is never written again.
    kept: AtomicUsize,
    first: [FaultCell; SEAT_FAULTS],
    stack_bytes: AtomicUsize,
    stack_measured: AtomicBool,
}

impl fmt::Debug for Seat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Seat").field("state", &self.state()).field("done", &self.done()).finish_non_exhaustive()
    }
}

impl Seat {
    #[must_use]
    pub fn state(&self) -> SeatState {
        SeatState::from_raw(self.state.load(Ordering::Acquire))
    }

    fn set(&self, state: SeatState) {
        self.state.store(state.raw(), Ordering::Release);
    }

    #[must_use]
    pub fn done(&self) -> u64 {
        self.done.load(Ordering::Acquire)
    }

    /// Mismatched words found, including those not kept.
    #[must_use]
    pub fn faults(&self) -> u64 {
        self.faults.load(Ordering::Acquire)
    }

    /// The kept faults from the `from`-th on, in the order the core found them.
    pub fn kept_from(&self, from: usize) -> impl Iterator<Item = Fault> + '_ {
        let kept = self.kept.load(Ordering::Acquire);
        self.first[from.min(kept)..kept].iter().map(|c| Fault {
            addr: CpuAddr::of(core::ptr::without_provenance::<u64>(c.addr.load(Ordering::Relaxed))),
            expected: c.expected.load(Ordering::Relaxed),
            actual: c.actual.load(Ordering::Relaxed),
        })
    }

    /// Forget the last test's progress and faults, once every core has finished it and before the
    /// next round begins.
    pub fn reset(&self) {
        self.gate.store(0, Ordering::Release);
        self.done.store(0, Ordering::Release);
        self.faults.store(0, Ordering::Release);
        self.kept.store(0, Ordering::Release);
    }

    /// Whether the core has finished the test of `round`, or has left its job for good.
    #[must_use]
    pub fn settled(&self, round: usize) -> bool {
        self.state().ended() || self.finished.load(Ordering::Acquire) >= round
    }

    /// Bytes of AP stack the job used below its dispatch entry; `None` on the BSP.
    #[must_use]
    pub fn stack_bytes(&self) -> Option<usize> {
        self.stack_measured.load(Ordering::Acquire).then(|| self.stack_bytes.load(Ordering::Relaxed))
    }

    fn record(&self, fault: Fault) {
        let i = self.kept.load(Ordering::Relaxed);
        if let Some(cell) = self.first.get(i) {
            cell.addr.store(fault.addr.get(), Ordering::Relaxed);
            cell.expected.store(fault.expected, Ordering::Relaxed);
            cell.actual.store(fault.actual, Ordering::Relaxed);
            self.kept.store(i + 1, Ordering::Release);
        }
    }

    fn room(&self) -> u64 {
        SEAT_FAULTS.saturating_sub(self.kept.load(Ordering::Relaxed)) as u64
    }
}

const START_WAITING: u8 = 0;
const START_GO: u8 = 1;
const START_ABANDONED: u8 = 2;

/// How an AP's wait at the start ended.
enum Start {
    Go,
    Abandoned,
    TimedOut,
}

/// What an AP waiting between tests found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// The round to work on.
    Round(usize),
    Closed,
    Stopped,
}

/// What the cores of one run of tests share: the start, the rounds, the gates and the stop.
#[derive(Debug, Default)]
pub struct Crew {
    arrived: AtomicU32,
    start: AtomicU8,
    stop: AtomicBool,
    /// The last round begun: round `n` is the `n`-th test of the run, from 1.
    round: AtomicUsize,
    /// No round will begin again.
    closed: AtomicBool,
    /// The phase of the last gate opened in this round; every core may start phases up to it.
    open: AtomicUsize,
}

impl Crew {
    /// APs waiting at the start.
    #[must_use]
    pub fn arrived(&self) -> u32 {
        self.arrived.load(Ordering::Acquire)
    }

    /// Let the waiting APs go.
    pub fn release(&self) {
        self.start.store(START_GO, Ordering::Release);
    }

    /// Send the waiting APs home, and any that arrive later.
    pub fn abandon(&self) {
        self.start.store(START_ABANDONED, Ordering::Release);
        self.stop();
    }

    /// Let every core go on to the next test, once all have finished the last and the seats are
    /// [reset](Seat::reset).
    pub fn begin_round(&self) {
        self.open.store(0, Ordering::Release);
        self.round.fetch_add(1, Ordering::AcqRel);
    }

    /// The last round begun, 0 before the first.
    #[must_use]
    pub fn round(&self) -> usize {
        self.round.load(Ordering::Acquire)
    }

    /// No round follows the last: every core ends its job once it has finished it.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    /// Every core ends its job at its next turn.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    fn arrive_and_wait(&self, clock: &impl Clock, patience: Duration) -> Start {
        self.arrived.fetch_add(1, Ordering::AcqRel);
        let deadline = clock.now().checked_add(patience);
        loop {
            match self.start.load(Ordering::Acquire) {
                START_WAITING => {},
                START_GO => return Start::Go,
                _ => return Start::Abandoned,
            }
            if deadline.is_none_or(|d| clock.now() > d) {
                return Start::TimedOut;
            }
            core::hint::spin_loop();
        }
    }

    /// Wait for a round after `seen`, the close or the stop.
    fn next_round(&self, seen: usize) -> Next {
        loop {
            if self.stopped() {
                return Next::Stopped;
            }
            let round = self.round();
            if round > seen {
                return Next::Round(round);
            }
            if self.closed.load(Ordering::Acquire) {
                return Next::Closed;
            }
            core::hint::spin_loop();
        }
    }

    /// Open the gate that every seat still working waits at, once all of them wait there. Returns
    /// whether every seat has finished the round's test or ended.
    pub fn coordinate<'s>(&self, seats: impl IntoIterator<Item = &'s Seat>) -> bool {
        let round = self.round();
        let open = self.open.load(Ordering::Acquire);
        let mut at = None;
        for seat in seats {
            if seat.settled(round) {
                continue;
            }
            let gate = seat.gate.load(Ordering::Acquire);
            if gate <= open || at.is_some_and(|g| g != gate) {
                return false;
            }
            at = Some(gate);
        }
        if let Some(gate) = at {
            self.open.store(gate, Ordering::Release);
        }
        at.is_none()
    }
}

/// One core's job for a run of tests: its test state for each, the regions of its share, the shared
/// crew and its own seat.
pub struct Job<'a, C> {
    /// The core's share of each test of the run, in order; test `n` is round `n + 1`.
    tests: Vec<UnsafeCell<Test>>,
    regions: UnsafeCell<Regions<'a>>,
    kernels: Kernels,
    budget_bytes: usize,
    clock: C,
    crew: &'a Crew,
    seat: &'a Seat,
    taken: AtomicBool,
}

// SAFETY: the tests and regions are touched only through the one Worker that wins `taken`;
// everything else is read-only or atomic.
unsafe impl<C: Sync> Sync for Job<'_, C> {}

impl<C> fmt::Debug for Job<'_, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Job").field("seat", self.seat).finish_non_exhaustive()
    }
}

impl<'a, C: Clock> Job<'a, C> {
    /// `tests` must be the tests of `regions`' share ([`Test::for_share`]), one per round.
    pub fn new(
        tests: Vec<Test>,
        regions: Regions<'a>,
        kernels: Kernels,
        budget_bytes: usize,
        clock: C,
        crew: &'a Crew,
        seat: &'a Seat,
    ) -> Self {
        Self {
            tests: tests.into_iter().map(UnsafeCell::new).collect(),
            regions: UnsafeCell::new(regions),
            kernels,
            budget_bytes,
            clock,
            crew,
            seat,
            taken: AtomicBool::new(false),
        }
    }

    fn take(&self) -> Option<Worker<'_, 'a, C>> {
        (!self.taken.swap(true, Ordering::AcqRel)).then_some(Worker { job: self, round: 0, gate: None, until: None })
    }

    /// Take the job on the calling core, once the start is released; `None` if already taken.
    pub fn claim(&self) -> Option<Worker<'_, 'a, C>> {
        let worker = self.take()?;
        self.seat.set(SeatState::Running);
        Some(worker)
    }
}

/// What one turn of a job did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Turn {
    Worked,
    /// Waiting at a gate for the other cores.
    Gate,
    /// Nothing to do before this: a bit-fade wait or a bus-stress pause.
    Wait(Instant),
    /// The round's test is finished on this core.
    Done,
    Stopped,
}

/// The one handle that works a job; it stays on the core that took it.
pub struct Worker<'j, 'a, C> {
    job: &'j Job<'a, C>,
    /// The round being worked, 0 before the first.
    round: usize,
    /// The phase this core waits to start.
    gate: Option<usize>,
    until: Option<Instant>,
}

impl<C: Clock> Worker<'_, '_, C> {
    /// Start working the test of `round`.
    pub const fn begin(&mut self, round: usize) {
        self.round = round;
        self.gate = None;
        self.until = None;
    }

    /// Leave the job for good: the crew is closed and the last test is finished.
    pub fn retire(&self) {
        self.job.seat.set(SeatState::Done);
    }

    /// One bounded step of the job, or a quick look at the gate or the clock while waiting. Wide
    /// kernels run inside `vector`'s guard.
    pub fn turn(&mut self, vector: Option<&dyn WideGuard>) -> Turn {
        let job = self.job;
        let (crew, seat) = (job.crew, job.seat);
        if crew.stopped() {
            seat.set(SeatState::Stopped);
            return Turn::Stopped;
        }
        if let Some(gate) = self.gate {
            if crew.open.load(Ordering::Acquire) < gate {
                return Turn::Gate;
            }
            self.gate = None;
        }
        let now = job.clock.now();
        if let Some(until) = self.until {
            if now < until {
                return Turn::Wait(until);
            }
            self.until = None;
        }
        let Some(cell) = self.round.checked_sub(1).and_then(|i| job.tests.get(i)) else {
            return Turn::Done;
        };
        // SAFETY: this worker is the job's only one (`take`), and it is not shared.
        let (test, regions) = unsafe { (&mut *cell.get(), &mut *job.regions.get()) };
        let phase = test.phase();
        let mut sink = |f: Fault| seat.record(f);
        let mut faults = Faults::new(seat.room(), &mut sink);
        let kernels = &job.kernels;
        let mut progress = Progress::Running;
        let mut step = || progress = test.step(regions, kernels, job.budget_bytes, now, &mut faults);
        match vector {
            Some(v) if kernels.is_wide() => v.wide(&mut step),
            _ => step(),
        }
        let seen = faults.seen();
        if seen > 0 {
            seat.faults.fetch_add(seen, Ordering::AcqRel);
        }
        seat.done.store(test.done(), Ordering::Release);
        match progress {
            Progress::Done => {
                seat.finished.store(self.round, Ordering::Release);
                Turn::Done
            },
            Progress::Waiting { until } => {
                self.until = Some(until);
                Turn::Wait(until)
            },
            Progress::Running if test.phase() != phase && test.gated() => {
                self.gate = Some(test.phase());
                seat.gate.store(test.phase(), Ordering::Release);
                Turn::Gate
            },
            Progress::Running => Turn::Worked,
        }
    }

    /// Work every round the crew begins, spinning between turns that have nothing to do, until it
    /// is closed or stopped; how the core left its job, which the seat then says too.
    pub fn serve(&mut self, vector: Option<&dyn WideGuard>) -> SeatState {
        let end = self.rounds(vector);
        self.job.seat.set(end);
        end
    }

    fn rounds(&mut self, vector: Option<&dyn WideGuard>) -> SeatState {
        let crew = self.job.crew;
        let mut seen = 0;
        loop {
            match crew.next_round(seen) {
                Next::Round(round) => seen = round,
                Next::Closed => return SeatState::Done,
                Next::Stopped => return SeatState::Stopped,
            }
            self.begin(seen);
            loop {
                match self.turn(vector) {
                    Turn::Worked => {},
                    Turn::Gate | Turn::Wait(_) => core::hint::spin_loop(),
                    Turn::Done => break,
                    Turn::Stopped => return SeatState::Stopped,
                }
            }
        }
    }
}

impl<C: Clock> Job<'_, C> {
    /// The AP's side of a run: check the core, wait at the start for the BSP's release, then
    /// [serve](Worker::serve) every round until the crew closes or stops. How it ended is in the
    /// seat. Allocates nothing and takes no lock, so it may run where only the core's own state is safe.
    pub fn run_ap<P: Platform>(&self, platform: &P) {
        if !platform.sse_ready() {
            return self.seat.set(SeatState::Failed(Failure::NoSse));
        }
        let Some(mut worker) = self.take() else {
            return self.seat.set(SeatState::Failed(Failure::Taken));
        };
        let k = self.kernels;
        let vector = if k.is_wide() { platform.enable_wide() } else { None };
        if ![k.fill, k.verify, k.rmw].iter().all(|k| platform.tier_usable(k.tier())) {
            return self.seat.set(SeatState::Failed(Failure::Tier));
        }
        self.seat.set(SeatState::Ready);
        match self.crew.arrive_and_wait(&self.clock, AP_PATIENCE) {
            Start::Go => {},
            Start::Abandoned => return self.seat.set(SeatState::Stopped),
            Start::TimedOut => return self.seat.set(SeatState::Failed(Failure::StartLine)),
        }
        self.seat.set(SeatState::Running);
        worker.serve(vector.as_ref().map(|v| v as &dyn WideGuard));
        drop(vector);
    }

    /// Record how deep the AP's stack went, once its run has ended.
    pub fn record_stack(&self, bytes: usize) {
        self.seat.stack_bytes.store(bytes, Ordering::Relaxed);
        self.seat.stack_measured.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests;
