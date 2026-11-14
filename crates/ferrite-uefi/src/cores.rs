//! Memory-test blocks on several cores at once, each core on its own slice of one region.
//!
//! The BSP cuts the region into disjoint block-aligned slices with [`with_split`], gives each core a
//! [`CoreJob`], starts the AP jobs through `ferrite_efi::mp`, runs its own, and reads the per-core
//! [`Slot`]s, which hold only atomics. Every core starts at one [`StartLine`], so the time from its
//! release to the last core's end is what the whole region took. The start line, the checks an AP
//! makes before its job, and the seat each core reports through are `ferrite_mem::crew`'s, shared
//! with the memory sweep.

use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::num::NonZeroUsize;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use core::time::Duration;

use ferrite_efi::crew::ThisCore;
use ferrite_efi::mp::{ApTask, StackUse};
use ferrite_hal::{Clock, Instant};
use ferrite_mem::crew::{Failure, Seat, SeatState, StartLine, WideGuard, enter_ap, guarded};
use ferrite_mem::{Fault, Faults, Inversion, Kernel, Order, Prsg, Region};

/// The first faults each core keeps with their addresses; the rest are only counted.
pub const SLOT_FAULTS: usize = 4;

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

/// One core's results, written only by that core and read by the BSP after the dispatch ended.
/// The seat holds its state, the bytes covered, the faults and the stack; the rest is the benchmark's.
#[derive(Debug, Default)]
pub struct Slot {
    seat: Seat,
    passes: AtomicU32,
    ended_ns: AtomicU64,
}

impl Slot {
    #[must_use]
    pub fn report(&self) -> CoreReport {
        let ended = match self.ended_ns.load(Ordering::Relaxed) {
            0 => None,
            ns => Some(Instant::from_epoch(Duration::from_nanos(ns))),
        };
        CoreReport {
            state: self.seat.state(),
            passes: self.passes.load(Ordering::Relaxed),
            bytes: self.seat.done(),
            faults: self.seat.faults(),
            ended,
            stack_bytes: self.seat.stack_bytes(),
            first: self.seat.kept_from(0).collect(),
        }
    }
}

/// What a [`Slot`] held when the BSP read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoreReport {
    pub state: SeatState,
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
            done: reports.iter().filter(|r| r.state == SeatState::Done).count(),
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
    pub fn run_here(&self, vector: Option<&dyn WideGuard>, duties: &mut dyn FnMut()) {
        if self.taken.swap(true, Ordering::AcqRel) {
            self.slot.seat.set(SeatState::Failed(Failure::Taken));
            return;
        }
        // SAFETY: `taken` makes this core the slice's only user.
        let slice = unsafe { &mut *self.slice.get() };
        self.work(slice, vector, duties);
    }

    fn work(&self, slice: &mut Slice<'a>, vector: Option<&dyn WideGuard>, between: &mut dyn FnMut()) {
        let Spec { kernel, work, passes, chunk_words, ref clock } = self.spec;
        let (slot, seat) = (self.slot, &self.slot.seat);
        seat.set(SeatState::Running);
        let mut sink = |f: Fault| seat.record(f);
        let mut faults = Faults::new(SLOT_FAULTS as u64, &mut sink);
        let mut bytes = 0_u64;
        for pass in 0..passes {
            let mut progress = |chunk: usize, faults: &Faults<'_>| {
                bytes = bytes.wrapping_add(chunk as u64);
                seat.set_done(bytes);
                seat.set_faults(faults.seen());
                between();
            };
            match work {
                Work::Copy => {
                    let Slice { region, source: Some(source), .. } = slice else {
                        seat.set(SeatState::Failed(Failure::Share));
                        return;
                    };
                    for (s, mut d) in source.chunks(chunk_words).zip(region.chunks(chunk_words)) {
                        let mut copied = Ok(());
                        under(vector, kernel, || copied = ferrite_mem::copy(kernel, &s, &mut d));
                        if copied.is_err() {
                            seat.set(SeatState::Failed(Failure::Share));
                            return;
                        }
                        progress(s.bytes() + d.bytes(), &faults);
                    }
                },
                Work::Fill { value } => {
                    for mut c in slice.region.chunks(chunk_words) {
                        under(vector, kernel, || ferrite_mem::fill(kernel, &mut c, value));
                        progress(c.bytes(), &faults);
                    }
                },
                Work::Verify { value } => {
                    for c in slice.region.chunks(chunk_words) {
                        under(vector, kernel, || ferrite_mem::verify(kernel, &c, value, &mut faults));
                        progress(c.bytes(), &faults);
                    }
                },
                Work::FillPrsg { seed } => {
                    let mut prsg = Prsg::at(seed, slice.block);
                    for mut c in slice.region.chunks(chunk_words) {
                        under(vector, kernel, || ferrite_mem::fill_prsg(kernel, &mut c, &mut prsg, 0));
                        progress(c.bytes(), &faults);
                    }
                },
                Work::VerifyPrsg { seed } => {
                    let mut prsg = Prsg::at(seed, slice.block);
                    for c in slice.region.chunks(chunk_words) {
                        under(vector, kernel, || ferrite_mem::verify_prsg(kernel, &c, &mut prsg, 0, &mut faults));
                        progress(c.bytes(), &faults);
                    }
                },
                Work::Invert { pattern } => {
                    let (expect, write) = if pass % 2 == 0 { (pattern, !pattern) } else { (!pattern, pattern) };
                    let step = Inversion { expect, write, order: Order::Ascending };
                    for mut c in slice.region.chunks(chunk_words) {
                        under(vector, kernel, || ferrite_mem::invert(kernel, &mut c, step, &mut faults));
                        progress(c.bytes(), &faults);
                    }
                },
            }
            slot.passes.store(pass.saturating_add(1), Ordering::Relaxed);
        }
        let ended = u64::try_from(clock.now().since_epoch().as_nanos()).unwrap_or(u64::MAX).max(1);
        slot.ended_ns.store(ended, Ordering::Relaxed);
        seat.set_faults(faults.seen());
        seat.set(SeatState::Done);
    }
}

/// Run `f` under the vector guard when the kernel is wide.
fn under(vector: Option<&dyn WideGuard>, kernel: Kernel, f: impl FnOnce()) {
    let mut f = Some(f);
    guarded(vector, kernel.tier().is_wide(), &mut || {
        if let Some(f) = f.take() {
            f();
        }
    });
}

// SAFETY: the AP path runs ferrite-mem kernels on its own slice, reads CPUID, CR4, XCR0 and the TSC,
// writes CR4 and XCR0 through VectorState (restored before return) and stores to atomics. No EFI
// call, allocation, logging or lock; the frames are a few hundred bytes deep.
unsafe impl<C: Clock + Sync> ApTask for CoreJob<'_, C> {
    fn run(&self) {
        let kernels = [self.spec.kernel];
        let Some(entered) = enter_ap(&ThisCore, &self.slot.seat, &self.taken, &kernels, self.line, &self.spec.clock)
        else {
            return;
        };
        // SAFETY: `taken` makes this AP the slice's only user.
        let slice = unsafe { &mut *self.slice.get() };
        self.work(slice, entered.guard(), &mut || {});
    }

    fn finish(&self, stack: StackUse) {
        self.slot.seat.record_stack(stack.bytes());
    }
}

#[cfg(test)]
mod tests;
