//! Memory-test blocks on several cores at once, each core on its own slice of one region.
//!
//! The BSP cuts the region into disjoint block-aligned slices with [`with_split`], gives each core a
//! [`CoreJob`], starts the AP jobs through `ferrite_efi::mp`, runs its own, and reads the per-core
//! [`Slot`]s, which hold only atomics. Every core starts at one [`StartLine`], so the time from its
//! release to the last core's end is what the whole region took.

use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::fmt;
use core::num::NonZeroUsize;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use core::time::Duration;

use ferrite_efi::cpu::{FpuControl, VectorState};
use ferrite_efi::crew::tier_usable;
use ferrite_efi::mp::{ApTask, StackUse};
use ferrite_hal::{Clock, Instant};
use ferrite_mem::{CpuAddr, Fault, Faults, Inversion, Kernel, Order, Prsg, Region};

/// The first faults each core keeps with their addresses; the rest are only counted.
pub const SLOT_FAULTS: usize = 4;
/// How long an AP waits at the start line for the BSP's release before giving up.
pub const AP_PATIENCE: Duration = Duration::from_secs(5);

/// What every core does to its slice, once per pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Work {
    Fill {
        value: u64,
    },
    Verify {
        value: u64,
    },
    FillPrsg {
        seed: u64,
    },
    VerifyPrsg {
        seed: u64,
    },
    /// A moving-inversions step from `pattern` to its complement, and back on odd passes, so an
    /// even number of passes leaves `pattern`.
    Invert {
        pattern: u64,
    },
    /// The lower half of the region into the upper half, each core its share of both.
    Copy,
}

/// One core's share: the words it works on, the matching source words for a copy, and where the
/// share starts in the whole region's PRSG sequence.
#[derive(Debug)]
pub struct Slice<'r> {
    region: Region<'r>,
    source: Option<Region<'r>>,
    block: u64,
}

impl<'r> Slice<'r> {
    #[must_use]
    pub const fn region(&self) -> &Region<'r> {
        &self.region
    }

    #[must_use]
    pub const fn source(&self) -> Option<&Region<'r>> {
        self.source.as_ref()
    }

    /// Bytes one pass touches: the slice, plus the source for a copy.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.region.bytes() + self.source.as_ref().map_or(0, Region::bytes)
    }
}

/// Cut `region` into at most `parts` slices of whole blocks and pass them to `f`.
///
/// The slices are consecutive and equal but for a shorter last one. For a copy they cover the upper
/// half, each paired with the same share of the lower half (an odd last block is left out).
pub fn with_split<R>(
    region: &mut Region<'_>,
    parts: NonZeroUsize,
    copy: bool,
    f: impl FnOnce(Vec<Slice<'_>>) -> R,
) -> R {
    if copy {
        let half = region.words() / 2;
        let (mut src, mut upper) = region.split_at(half);
        let mut dst = upper.sub(0, src.words());
        let step = step_words(src.blocks(), parts);
        let slices = src
            .chunks(step)
            .zip(dst.chunks(step))
            .map(|(source, region)| Slice { region, source: Some(source), block: 0 })
            .collect();
        f(slices)
    } else {
        let step = step_words(region.blocks(), parts);
        let slices = region
            .chunks(step)
            .enumerate()
            .map(|(i, region)| Slice { region, source: None, block: (i * step / ferrite_mem::BLOCK_WORDS) as u64 })
            .collect();
        f(slices)
    }
}

fn step_words(blocks: usize, parts: NonZeroUsize) -> usize {
    blocks.div_ceil(parts.get()).max(1) * ferrite_mem::BLOCK_WORDS
}

/// Why a core did not run its passes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The firmware left SSE off on this AP.
    NoSse,
    /// The kernel's tier does not run on this core.
    Tier,
    /// The BSP's release did not come in time, or the BSP gave up on this round.
    StartLine,
    /// The job was already taken by another core.
    Taken,
    /// Source and destination differ in length.
    Copy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoreState {
    Idle,
    Waiting,
    Running,
    Done,
    Failed(Failure),
    Unknown(u8),
}

impl CoreState {
    const fn raw(self) -> u8 {
        match self {
            Self::Idle => 0,
            Self::Waiting => 1,
            Self::Running => 2,
            Self::Done => 3,
            Self::Failed(Failure::NoSse) => 0x10,
            Self::Failed(Failure::Tier) => 0x11,
            Self::Failed(Failure::StartLine) => 0x12,
            Self::Failed(Failure::Taken) => 0x13,
            Self::Failed(Failure::Copy) => 0x14,
            Self::Unknown(raw) => raw,
        }
    }

    const fn from_raw(raw: u8) -> Self {
        match raw {
            0 => Self::Idle,
            1 => Self::Waiting,
            2 => Self::Running,
            3 => Self::Done,
            0x10 => Self::Failed(Failure::NoSse),
            0x11 => Self::Failed(Failure::Tier),
            0x12 => Self::Failed(Failure::StartLine),
            0x13 => Self::Failed(Failure::Taken),
            0x14 => Self::Failed(Failure::Copy),
            raw => Self::Unknown(raw),
        }
    }
}

impl fmt::Display for CoreState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Idle => f.write_str("never started"),
            Self::Waiting => f.write_str("stopped before its first pass (timed out or panicked)"),
            Self::Running => f.write_str("stopped mid-run (timed out or panicked)"),
            Self::Done => f.write_str("done"),
            Self::Failed(Failure::NoSse) => f.write_str("failed: SSE disabled"),
            Self::Failed(Failure::Tier) => f.write_str("failed: tier unusable on this core"),
            Self::Failed(Failure::StartLine) => f.write_str("failed: no release at the start line"),
            Self::Failed(Failure::Taken) => f.write_str("failed: job already taken"),
            Self::Failed(Failure::Copy) => f.write_str("failed: copy lengths differ"),
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

/// One core's results, written only by that core and read by the BSP after the dispatch ended.
/// A cache line of its own, so no two cores share one.
#[derive(Default)]
#[repr(align(64))]
pub struct Slot {
    state: AtomicU8,
    passes: AtomicU32,
    bytes: AtomicU64,
    faults: AtomicU64,
    ended_ns: AtomicU64,
    stack_bytes: AtomicUsize,
    stack_measured: AtomicBool,
    first: [FaultCell; SLOT_FAULTS],
}

impl fmt::Debug for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Slot")
            .field("state", &CoreState::from_raw(self.state.load(Ordering::Relaxed)))
            .finish_non_exhaustive()
    }
}

impl Slot {
    fn set(&self, state: CoreState) {
        self.state.store(state.raw(), Ordering::Release);
    }

    fn record(&self, index: usize, fault: Fault) {
        if let Some(cell) = self.first.get(index) {
            cell.addr.store(fault.addr.get(), Ordering::Relaxed);
            cell.expected.store(fault.expected, Ordering::Relaxed);
            cell.actual.store(fault.actual, Ordering::Relaxed);
        }
    }

    #[must_use]
    pub fn report(&self) -> CoreReport {
        let state = CoreState::from_raw(self.state.load(Ordering::Acquire));
        let faults = self.faults.load(Ordering::Relaxed);
        let kept = usize::try_from(faults).unwrap_or(usize::MAX).min(SLOT_FAULTS);
        let first = self.first[..kept]
            .iter()
            .map(|c| Fault {
                addr: CpuAddr::of(core::ptr::without_provenance::<u64>(c.addr.load(Ordering::Relaxed))),
                expected: c.expected.load(Ordering::Relaxed),
                actual: c.actual.load(Ordering::Relaxed),
            })
            .collect();
        let ended = match self.ended_ns.load(Ordering::Relaxed) {
            0 => None,
            ns => Some(Instant::from_epoch(Duration::from_nanos(ns))),
        };
        CoreReport {
            state,
            passes: self.passes.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            faults,
            ended,
            stack_bytes: self.stack_measured.load(Ordering::Acquire).then(|| self.stack_bytes.load(Ordering::Relaxed)),
            first,
        }
    }
}

/// What a [`Slot`] held when the BSP read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoreReport {
    pub state: CoreState,
    pub passes: u32,
    /// Bytes covered over every pass.
    pub bytes: u64,
    /// Mismatched words over every pass.
    pub faults: u64,
    /// When the core finished its last pass.
    pub ended: Option<Instant>,
    /// Stack depth below the AP's dispatch entry; `None` on the BSP.
    pub stack_bytes: Option<usize>,
    pub first: Vec<Fault>,
}

/// All cores' reports for one round, added up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Summary {
    pub cores: usize,
    pub done: usize,
    pub bytes: u64,
    pub faults: u64,
    /// From the start line's release to the last core's end.
    pub span: Duration,
    /// The deepest AP stack measured.
    pub stack_bytes: Option<usize>,
}

impl Summary {
    #[must_use]
    pub fn of(released: Instant, reports: &[CoreReport]) -> Self {
        let last = reports.iter().filter_map(|r| r.ended).max().unwrap_or(released);
        Self {
            cores: reports.len(),
            done: reports.iter().filter(|r| r.state == CoreState::Done).count(),
            bytes: reports.iter().map(|r| r.bytes).sum(),
            faults: reports.iter().map(|r| r.faults).sum(),
            span: last - released,
            stack_bytes: reports.iter().filter_map(|r| r.stack_bytes).max(),
        }
    }

    #[must_use]
    pub const fn complete(&self) -> bool {
        self.done == self.cores
    }

    /// Aggregate GB/s (bytes per nanosecond).
    #[must_use]
    #[expect(
        clippy::cast_precision_loss,
        reason = "a rate for display; GB-scale byte counts and ns durations stay well inside f64"
    )]
    pub fn rate(&self) -> f64 {
        self.bytes as f64 / self.span.as_nanos().max(1) as f64
    }
}

const LINE_WAITING: u8 = 0;
const LINE_GO: u8 = 1;
const LINE_ABANDONED: u8 = 2;

/// Where the cores of one round wait for each other, so they all start together.
#[derive(Debug, Default)]
pub struct StartLine {
    arrived: AtomicU32,
    signal: AtomicU8,
}

impl StartLine {
    /// AP side: report arrival, then wait for the release; false if abandoned or out of patience.
    fn arrive_and_wait(&self, clock: &impl Clock, patience: Duration) -> bool {
        self.arrived.fetch_add(1, Ordering::AcqRel);
        let deadline = clock.now().checked_add(patience);
        loop {
            match self.signal.load(Ordering::Acquire) {
                LINE_WAITING => {},
                LINE_GO => return true,
                _ => return false,
            }
            if deadline.is_none_or(|d| clock.now() > d) {
                return false;
            }
            core::hint::spin_loop();
        }
    }

    /// BSP side: wait up to `patience` for `aps` arrivals, running `duties` meanwhile, then release
    /// them. Returns the release time, or `None` (and abandons the round) if some never arrived.
    pub fn gather(
        &self,
        aps: u32,
        clock: &impl Clock,
        patience: Duration,
        duties: &mut dyn FnMut(),
    ) -> Option<Instant> {
        let deadline = clock.now().checked_add(patience);
        while self.arrived.load(Ordering::Acquire) < aps {
            if deadline.is_none_or(|d| clock.now() > d) {
                self.signal.store(LINE_ABANDONED, Ordering::Release);
                return None;
            }
            duties();
            core::hint::spin_loop();
        }
        let now = clock.now();
        self.signal.store(LINE_GO, Ordering::Release);
        Some(now)
    }
}

/// How every core of a round works.
#[derive(Clone, Copy, Debug)]
pub struct Spec<C> {
    pub kernel: Kernel,
    pub work: Work,
    pub passes: u32,
    pub chunk_words: usize,
    pub clock: C,
}

/// One core's job: its slice, the shared spec and start line, and its own result slot.
pub struct CoreJob<'a, C> {
    spec: Spec<C>,
    slice: UnsafeCell<Slice<'a>>,
    line: &'a StartLine,
    slot: &'a Slot,
    taken: AtomicBool,
}

// SAFETY: the slice is touched only by the one core that wins `taken`; everything else is read-only
// or atomic.
unsafe impl<C: Sync> Sync for CoreJob<'_, C> {}

impl<'a, C: Clock> CoreJob<'a, C> {
    pub const fn new(spec: Spec<C>, slice: Slice<'a>, line: &'a StartLine, slot: &'a Slot) -> Self {
        Self { spec, slice: UnsafeCell::new(slice), line, slot, taken: AtomicBool::new(false) }
    }

    /// Run on the calling core, which releases the start line itself: `duties` runs after each chunk.
    pub fn run_here(&self, vector: Option<&VectorState>, duties: &mut dyn FnMut()) {
        if self.taken.swap(true, Ordering::AcqRel) {
            self.slot.set(CoreState::Failed(Failure::Taken));
            return;
        }
        // SAFETY: `taken` makes this core the slice's only user.
        let slice = unsafe { &mut *self.slice.get() };
        self.work(slice, vector, duties);
    }

    fn work(&self, slice: &mut Slice<'a>, vector: Option<&VectorState>, between: &mut dyn FnMut()) {
        let Spec { kernel, work, passes, chunk_words, ref clock } = self.spec;
        let slot = self.slot;
        slot.set(CoreState::Running);
        let mut kept = 0;
        let mut sink = |f: Fault| {
            slot.record(kept, f);
            kept = kept.saturating_add(1);
        };
        let mut faults = Faults::new(SLOT_FAULTS as u64, &mut sink);
        let mut bytes = 0_u64;
        for pass in 0..passes {
            let mut progress = |chunk: usize, faults: &Faults<'_>| {
                bytes = bytes.wrapping_add(chunk as u64);
                slot.bytes.store(bytes, Ordering::Relaxed);
                slot.faults.store(faults.seen(), Ordering::Relaxed);
                between();
            };
            match work {
                Work::Copy => {
                    let Slice { region, source: Some(source), .. } = slice else {
                        slot.set(CoreState::Failed(Failure::Copy));
                        return;
                    };
                    for (s, mut d) in source.chunks(chunk_words).zip(region.chunks(chunk_words)) {
                        let mut copied = Ok(());
                        guarded(vector, kernel, || copied = ferrite_mem::copy(kernel, &s, &mut d));
                        if copied.is_err() {
                            slot.set(CoreState::Failed(Failure::Copy));
                            return;
                        }
                        progress(s.bytes() + d.bytes(), &faults);
                    }
                },
                Work::Fill { value } => {
                    for mut c in slice.region.chunks(chunk_words) {
                        guarded(vector, kernel, || ferrite_mem::fill(kernel, &mut c, value));
                        progress(c.bytes(), &faults);
                    }
                },
                Work::Verify { value } => {
                    for c in slice.region.chunks(chunk_words) {
                        guarded(vector, kernel, || ferrite_mem::verify(kernel, &c, value, &mut faults));
                        progress(c.bytes(), &faults);
                    }
                },
                Work::FillPrsg { seed } => {
                    let mut prsg = Prsg::at(seed, slice.block);
                    for mut c in slice.region.chunks(chunk_words) {
                        guarded(vector, kernel, || ferrite_mem::fill_prsg(kernel, &mut c, &mut prsg, 0));
                        progress(c.bytes(), &faults);
                    }
                },
                Work::VerifyPrsg { seed } => {
                    let mut prsg = Prsg::at(seed, slice.block);
                    for c in slice.region.chunks(chunk_words) {
                        guarded(vector, kernel, || ferrite_mem::verify_prsg(kernel, &c, &mut prsg, 0, &mut faults));
                        progress(c.bytes(), &faults);
                    }
                },
                Work::Invert { pattern } => {
                    let (expect, write) = if pass % 2 == 0 { (pattern, !pattern) } else { (!pattern, pattern) };
                    let step = Inversion { expect, write, order: Order::Ascending };
                    for mut c in slice.region.chunks(chunk_words) {
                        guarded(vector, kernel, || ferrite_mem::invert(kernel, &mut c, step, &mut faults));
                        progress(c.bytes(), &faults);
                    }
                },
            }
            slot.passes.store(pass.saturating_add(1), Ordering::Relaxed);
        }
        let ended = u64::try_from(clock.now().since_epoch().as_nanos()).unwrap_or(u64::MAX).max(1);
        slot.ended_ns.store(ended, Ordering::Relaxed);
        slot.faults.store(faults.seen(), Ordering::Relaxed);
        slot.set(CoreState::Done);
    }

    fn fail(&self, failure: Failure) {
        self.slot.set(CoreState::Failed(failure));
    }
}

fn guarded(vector: Option<&VectorState>, kernel: Kernel, f: impl FnOnce()) {
    match vector {
        Some(v) if kernel.tier().is_wide() => v.wide(f),
        _ => f(),
    }
}

// SAFETY: the AP path runs ferrite-mem kernels on its own slice, reads CPUID, CR4, XCR0 and the TSC,
// writes CR4 and XCR0 through VectorState (restored before return) and stores to atomics. No EFI
// call, allocation, logging or lock; the frames are a few hundred bytes deep.
unsafe impl<C: Clock + Sync> ApTask for CoreJob<'_, C> {
    fn run(&self) {
        if !FpuControl::read().sse_ready() {
            return self.fail(Failure::NoSse);
        }
        if self.taken.swap(true, Ordering::AcqRel) {
            return self.fail(Failure::Taken);
        }
        let tier = self.spec.kernel.tier();
        let vector = if tier.is_wide() { VectorState::enable().ok() } else { None };
        if !tier_usable(tier) {
            return self.fail(Failure::Tier);
        }
        self.slot.set(CoreState::Waiting);
        if !self.line.arrive_and_wait(&self.spec.clock, AP_PATIENCE) {
            return self.fail(Failure::StartLine);
        }
        // SAFETY: `taken` makes this AP the slice's only user.
        let slice = unsafe { &mut *self.slice.get() };
        self.work(slice, vector.as_ref(), &mut || {});
        drop(vector);
    }

    fn finish(&self, stack: StackUse) {
        self.slot.stack_bytes.store(stack.bytes(), Ordering::Relaxed);
        self.slot.stack_measured.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests;
