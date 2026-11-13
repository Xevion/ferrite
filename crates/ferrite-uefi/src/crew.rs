//! One test of the memory suite on several cores at once, each core on its own share of the region.
//!
//! The BSP gives each core a [`Job`]: the core's [`Test`] state machine and the region of its share,
//! so no test state lives on a core's stack. AP jobs start through `ferrite_efi::mp`; the BSP works
//! its own job a turn at a time between its other duties. The cores meet at the start and at every
//! gated phase boundary ([`Test::gated`]), where each waits until all have ended the phase: the BSP
//! opens the gate ([`Crew::coordinate`]). Each core reports through its [`Seat`], which holds only
//! atomics.

use core::cell::UnsafeCell;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use core::time::Duration;

use ferrite_efi::cpu::{FpuControl, VectorState};
use ferrite_efi::mp::{ApTask, StackUse};
use ferrite_hal::{Clock, Instant};
use ferrite_mem::suite::{Kernels, Progress, Test};
use ferrite_mem::{CpuAddr, Fault, Faults, Region};

use crate::cores::tier_usable;

/// The first faults each core keeps per test with their addresses; the rest are only counted.
pub const SEAT_FAULTS: usize = 16;
/// How long an AP waits at the start for the BSP's release before giving up.
pub const AP_PATIENCE: Duration = Duration::from_secs(5);

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
    /// Work done, in the test's units.
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

/// What the cores of one test share: the start, the gates and the stop.
#[derive(Debug, Default)]
pub struct Crew {
    arrived: AtomicU32,
    start: AtomicU8,
    stop: AtomicBool,
    /// The phase of the last gate opened; every core may start phases up to it.
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

    /// Open the gate that every seat still working waits at, once all of them wait there. Returns
    /// whether every seat has ended.
    pub fn coordinate<'s>(&self, seats: impl IntoIterator<Item = &'s Seat>) -> bool {
        let open = self.open.load(Ordering::Acquire);
        let mut at = None;
        for seat in seats {
            if seat.state().ended() {
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

/// One core's job for one test: its test state and share, the shared crew and its own seat.
pub struct Job<'a, C> {
    test: UnsafeCell<Test>,
    region: UnsafeCell<Region<'a>>,
    kernels: Kernels,
    budget_bytes: usize,
    clock: C,
    crew: &'a Crew,
    seat: &'a Seat,
    taken: AtomicBool,
}

// SAFETY: the test and region are touched only through the one Worker that wins `taken`; everything
// else is read-only or atomic.
unsafe impl<C: Sync> Sync for Job<'_, C> {}

impl<C> fmt::Debug for Job<'_, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Job").field("seat", self.seat).finish_non_exhaustive()
    }
}

impl<'a, C: Clock> Job<'a, C> {
    /// `test` must be the test of `region`'s share ([`Test::for_share`]).
    pub const fn new(
        test: Test,
        region: Region<'a>,
        kernels: Kernels,
        budget_bytes: usize,
        clock: C,
        crew: &'a Crew,
        seat: &'a Seat,
    ) -> Self {
        Self {
            test: UnsafeCell::new(test),
            region: UnsafeCell::new(region),
            kernels,
            budget_bytes,
            clock,
            crew,
            seat,
            taken: AtomicBool::new(false),
        }
    }

    fn take(&self) -> Option<Worker<'_, 'a, C>> {
        (!self.taken.swap(true, Ordering::AcqRel)).then_some(Worker { job: self, gate: None, until: None })
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
    Done,
    Stopped,
}

/// The one handle that works a job; it stays on the core that took it.
pub struct Worker<'j, 'a, C> {
    job: &'j Job<'a, C>,
    /// The phase this core waits to start.
    gate: Option<usize>,
    until: Option<Instant>,
}

impl<C: Clock> Worker<'_, '_, C> {
    /// One bounded step of the job, or a quick look at the gate or the clock while waiting. Wide
    /// kernels run inside `vector`'s guard.
    pub fn turn(&mut self, vector: Option<&VectorState>) -> Turn {
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
        // SAFETY: this worker is the job's only one (`take`), and it is not shared.
        let (test, region) = unsafe { (&mut *job.test.get(), &mut *job.region.get()) };
        let phase = test.phase();
        let mut sink = |f: Fault| seat.record(f);
        let mut faults = Faults::new(seat.room(), &mut sink);
        let kernels = &job.kernels;
        let mut step = || test.step(region, kernels, job.budget_bytes, now, &mut faults);
        let progress = match vector {
            Some(v) if kernels.is_wide() => v.wide(step),
            _ => step(),
        };
        let seen = faults.seen();
        if seen > 0 {
            seat.faults.fetch_add(seen, Ordering::AcqRel);
        }
        seat.done.store(test.done(), Ordering::Release);
        match progress {
            Progress::Done => {
                seat.set(SeatState::Done);
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
}

// SAFETY: the AP path runs ferrite-mem kernels on its own share, reads CPUID, CR4, XCR0 and the TSC,
// writes CR4 and XCR0 through VectorState (restored before return) and stores to atomics. No EFI
// call, allocation, logging or lock; the test's state lives in the job, so the frames stay small.
unsafe impl<C: Clock + Sync> ApTask for Job<'_, C> {
    fn run(&self) {
        if !FpuControl::read().sse_ready() {
            return self.seat.set(SeatState::Failed(Failure::NoSse));
        }
        let Some(mut worker) = self.take() else {
            return self.seat.set(SeatState::Failed(Failure::Taken));
        };
        let k = self.kernels;
        let vector = if k.is_wide() { VectorState::enable().ok() } else { None };
        if ![k.fill, k.verify, k.rmw].iter().all(|k| tier_usable(k.tier())) {
            return self.seat.set(SeatState::Failed(Failure::Tier));
        }
        self.seat.set(SeatState::Ready);
        match self.crew.arrive_and_wait(&self.clock, AP_PATIENCE) {
            Start::Go => {},
            Start::Abandoned => return self.seat.set(SeatState::Stopped),
            Start::TimedOut => return self.seat.set(SeatState::Failed(Failure::StartLine)),
        }
        self.seat.set(SeatState::Running);
        loop {
            match worker.turn(vector.as_ref()) {
                Turn::Worked => {},
                Turn::Gate | Turn::Wait(_) => core::hint::spin_loop(),
                Turn::Done | Turn::Stopped => break,
            }
        }
        drop(vector);
    }

    fn finish(&self, stack: StackUse) {
        self.seat.stack_bytes.store(stack.bytes(), Ordering::Relaxed);
        self.seat.stack_measured.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests;
