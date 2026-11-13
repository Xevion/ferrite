//! Running a test's phases chunk by chunk.

use core::time::Duration;

use ferrite_hal::Instant;

use super::plan::{self, Bus, MODULO, Op, Phase, Span, move_half};
use super::{Config, Kernels, TestKind, percent};
use crate::fault::Faults;
use crate::kernels::{self, Dram, Stride};
use crate::ops::{self, Order};
use crate::prsg::Prsg;
use crate::region::{BLOCK_WORDS, Region};
use crate::rotating::Rotating;

/// Bus stress pauses for `BUS_GAP` after every `BUS_ON_BYTES` of traffic on its duty-cycled sweeps.
const BUS_ON_BYTES: usize = 4 << 20;
const BUS_GAP: Duration = Duration::from_micros(1500);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    /// More to do; step again.
    Running,
    /// Nothing to do before `until`: a bit-fade wait, or a bus-stress pause.
    Waiting {
        until: Instant,
    },
    Done,
}

/// One test over one region, or over one core's share of it, as a sequence of phases advanced by
/// [`Test::step`].
#[derive(Debug)]
pub struct Test {
    kind: TestKind,
    config: Config,
    pass: u64,
    /// Words of the whole region the phases are planned over.
    words: usize,
    /// The words of it this test works on.
    share: Span,
    phases: usize,
    phase: usize,
    active: Option<Active>,
    /// The phase after the one that last ended, fetched to decide [`Test::gated`].
    next: Option<Phase>,
    gated: bool,
    waits: Duration,
    /// Work of the phases already done, and of all of them.
    done: u64,
    total: u64,
}

impl Test {
    /// The test for pass `pass` over `region`; every step must get the same region.
    #[must_use]
    pub fn new(kind: TestKind, config: Config, pass: u64, region: &Region<'_>) -> Self {
        let words = region.words();
        Self::for_share(kind, config, pass, words, Span { start: 0, words })
    }

    /// The test planned over a region of `words` words, working only on `share` of it: every step
    /// must get the region of `share` alone, whose word 0 is word `share.start` of the whole. The
    /// sequences (PRSG, shifting pattern, stride, move pattern) continue across shares, so the
    /// shares of one region together do what the whole-region test does.
    #[must_use]
    pub fn for_share(kind: TestKind, config: Config, pass: u64, words: usize, share: Span) -> Self {
        let m = plan::measure(kind, &config, pass, words, share);
        Self {
            kind,
            config,
            pass,
            words,
            share,
            phases: m.phases,
            phase: 0,
            active: None,
            next: None,
            gated: false,
            waits: m.waits,
            done: 0,
            total: m.work,
        }
    }

    /// Whether the phase the next step starts may only begin once every share has ended the one
    /// before. Only consecutive block-move windows go without, since each lies inside one share.
    #[must_use]
    pub const fn gated(&self) -> bool {
        self.gated
    }

    /// The time the test's phases spend waiting, in all.
    #[must_use]
    pub const fn waits(&self) -> Duration {
        self.waits
    }

    #[must_use]
    pub const fn kind(&self) -> TestKind {
        self.kind
    }

    /// Index of the phase the next step works on.
    #[must_use]
    pub const fn phase(&self) -> usize {
        self.phase
    }

    #[must_use]
    pub const fn phases(&self) -> usize {
        self.phases
    }

    /// Work done so far, in words.
    #[must_use]
    pub fn done(&self) -> u64 {
        self.done + self.active.as_ref().map_or(0, |a| a.cursor as u64)
    }

    #[must_use]
    pub const fn total(&self) -> u64 {
        self.total
    }

    /// Progress through the test, 0 to 100.
    #[must_use]
    pub fn percent(&self) -> u16 {
        percent(self.done(), self.total)
    }

    /// Do up to `budget_bytes` (at least one block) of the current phase. A step ends at the end of
    /// its phase, so the next one starts the next phase.
    pub fn step(
        &mut self,
        region: &mut Region<'_>,
        kernels: &Kernels,
        budget_bytes: usize,
        now: Instant,
        faults: &mut Faults<'_>,
    ) -> Progress {
        debug_assert!(region.words() == self.share.words, "a step gets its share's region");
        if self.phase >= self.phases {
            return Progress::Done;
        }
        if self.active.is_none() {
            let next = self.next.take();
            let Some(phase) = next.or_else(|| plan::nth(self.kind, &self.config, self.pass, self.words, self.phase))
            else {
                self.phase = self.phases;
                return Progress::Done;
            };
            self.active = Some(Active::start(phase, self.share));
            self.gated = false;
        }
        let Some(active) = self.active.as_mut() else { return Progress::Done };
        let budget = (budget_bytes / size_of::<u64>()).max(BLOCK_WORDS) / BLOCK_WORDS * BLOCK_WORDS;
        if active.work > 0
            && let Some(until) = active.run(region, kernels, budget, now, faults)
        {
            return Progress::Waiting { until };
        }
        if active.cursor < active.work {
            return Progress::Running;
        }
        self.done += active.work as u64;
        let windowed = matches!(active.phase.op, Op::Move { .. });
        self.active = None;
        self.phase += 1;
        if self.phase == self.phases {
            return Progress::Done;
        }
        self.next = plan::nth(self.kind, &self.config, self.pass, self.words, self.phase);
        self.gated = !(windowed && self.next.is_some_and(|p| matches!(p.op, Op::Move { .. })));
        Progress::Running
    }
}

/// The phase being worked on, with the sequence states it carries from chunk to chunk.
#[derive(Debug)]
struct Active {
    phase: Phase,
    /// The words of the phase's span inside the share; for a bus sweep, of its fill stream.
    span: Span,
    /// The words of a bus sweep's verify stream inside the share.
    check_span: Span,
    /// The share's first word, word 0 of the region each step gets.
    base: usize,
    work: usize,
    cursor: usize,
    prsg: Prsg,
    /// The bus-stress verify stream.
    check: Prsg,
    rot: Rotating,
    /// Words done of the bus-stress fill and verify streams.
    bus: [usize; 2],
    since_gap: usize,
    until: Option<Instant>,
}

impl Active {
    fn start(phase: Phase, share: Span) -> Self {
        let mut span = phase.span.clip(share);
        let mut check_span = Span { start: span.start, words: 0 };
        // The block of the sequence at which this share's walk of `part` (a part of `of`) begins.
        let entry = |part: Span, of: Span| {
            let word = if phase.order() == Order::Descending { part.end() } else { part.start };
            ((word - of.start) / BLOCK_WORDS) as u64
        };
        let (mut prsg, mut check, mut rot) = (Prsg::new(0), Prsg::new(0), Rotating::at(0, 0));
        match phase.op {
            Op::FillPrsg { seed, .. } | Op::VerifyPrsg { seed, .. } | Op::InvertPrsg { seed, .. } => {
                prsg = Prsg::at(seed, entry(span, phase.span));
            },
            Op::FillRotating { pattern, .. } | Op::InvertRotating { pattern, .. } => {
                rot = Rotating::at(pattern, entry(span, phase.span));
            },
            Op::Bus(bus) => {
                span = bus.fill.span.clip(share);
                prsg = Prsg::at(bus.fill.seed, entry(span, bus.fill.span));
                if let Some(c) = bus.check {
                    check_span = c.span.clip(share);
                    check = Prsg::at(c.seed, entry(check_span, c.span));
                }
            },
            _ => {},
        }
        Self {
            phase,
            span,
            check_span,
            base: share.start,
            work: phase.work_in(share),
            cursor: 0,
            prsg,
            check,
            rot,
            bus: [0; 2],
            since_gap: 0,
            until: None,
        }
    }

    /// Run one chunk of at most `budget` words, or return the time to wait until.
    fn run(
        &mut self,
        region: &mut Region<'_>,
        k: &Kernels,
        budget: usize,
        now: Instant,
        faults: &mut Faults<'_>,
    ) -> Option<Instant> {
        if let Some(until) = self.until {
            if now < until {
                return Some(until);
            }
            self.until = None;
            if let Op::Wait(_) = self.phase.op {
                self.cursor = self.work;
                return None;
            }
        }
        match self.phase.op {
            Op::Wait(d) => {
                let until = now + d;
                self.until = Some(until);
                return Some(until);
            },
            Op::AddressWalk { invert } => {
                address_walk(region, invert, faults);
                self.cursor = self.work;
            },
            Op::Move { .. } => self.block_move(region, *k, budget),
            Op::Bus(bus) => self.bus(region, *k, bus, budget, now, faults),
            Op::MoveInit => {
                let (start, n) = self.next_chunk(budget.next_multiple_of(2 * BLOCK_WORDS));
                let chunk = region.sub(start - self.base, n);
                // SAFETY: the chunk is a region, so its blocks are ours.
                unsafe { kernels::fill_move_pattern(chunk.ptr(), chunk.blocks(), start / (2 * BLOCK_WORDS)) };
            },
            _ => self.linear_op(region, k, budget, faults),
        }
        None
    }

    /// The next chunk of a phase that walks its span once, as its first word of the whole region
    /// and its length.
    fn next_chunk(&mut self, budget: usize) -> (usize, usize) {
        let n = budget.min(self.work - self.cursor);
        let span = self.span;
        let start = match self.phase.order() {
            Order::Ascending => span.start + self.cursor,
            Order::Descending => span.end() - self.cursor - n,
        };
        self.cursor += n;
        (start, n)
    }

    fn linear_op(&mut self, region: &mut Region<'_>, k: &Kernels, budget: usize, faults: &mut Faults<'_>) {
        let op = self.phase.op;
        let (start, n) = self.next_chunk(budget);
        let mut c = region.sub(start - self.base, n);
        let (prsg, rot) = (&mut self.prsg, &mut self.rot);
        let stride = |offset| Stride { n: MODULO, offset, first: start % MODULO };
        match op {
            Op::Fill(v) => ops::fill(k.fill, &mut c, v),
            Op::Verify(v) => ops::verify(k.verify, &c, v, faults),
            Op::Invert(step) => ops::invert(k.rmw, &mut c, step, faults),
            Op::FillAddress(key) => ops::fill_address(k.fill, &mut c, key),
            Op::VerifyAddress(key) => ops::verify_address(k.verify, &c, key, faults),
            Op::FillPrsg { key, .. } => ops::fill_prsg(k.fill, &mut c, prsg, key),
            Op::VerifyPrsg { key, .. } => ops::verify_prsg(k.verify, &c, prsg, key, faults),
            Op::InvertPrsg { step, .. } => ops::invert_prsg(k.rmw, &mut c, prsg, step, faults),
            Op::FillRotating { key, .. } => ops::fill_rotating(k.fill, &mut c, rot, key),
            Op::InvertRotating { step, .. } => ops::invert_rotating(k.rmw, &mut c, rot, step, faults),
            Op::FillExcept { offset, value } => ops::fill_except(k.fill, &mut c, stride(offset), value),
            // SAFETY: the chunk is a region, so its words are ours.
            Op::FillStride { offset, value } => unsafe {
                kernels::fill_stride(c.ptr(), c.words(), stride(offset), value);
            },
            Op::VerifyStride { offset, value } => {
                // SAFETY: as above.
                unsafe { kernels::verify_stride(c.ptr(), c.words(), stride(offset), value, faults) };
            },
            // SAFETY: as above.
            Op::VerifyPairs => unsafe { kernels::verify_pairs(c.ptr(), c.blocks(), faults) },
            Op::MoveInit | Op::Move { .. } | Op::AddressWalk { .. } | Op::Bus(_) | Op::Wait(_) => {},
        }
    }

    /// memtest86+'s block move over one window, `iterations` times: the low half is copied onto the
    /// high half, the high half less its last block onto the low half one block up, and that last
    /// block onto the start. Each copy is ascending, so it can be split into chunks.
    fn block_move(&mut self, region: &mut Region<'_>, k: Kernels, budget: usize) {
        let span = self.phase.span;
        let half = move_half(span);
        let r = self.cursor % (2 * half);
        // (copy from the high half, source offset, destination offset, words left in this copy)
        let (from_high, src, dst, left) = if r < half {
            (false, r, r, half - r)
        } else if r < 2 * half - BLOCK_WORDS {
            let o = r - half;
            (true, o, BLOCK_WORDS + o, half - BLOCK_WORDS - o)
        } else {
            let o = r - (2 * half - BLOCK_WORDS);
            (true, half - BLOCK_WORDS + o, o, BLOCK_WORDS - o)
        };
        let n = left.min(budget);
        let mut window = region.sub(span.start - self.base, 2 * half);
        let (mut low, mut high) = window.split_at(half);
        let copied = if from_high {
            ops::copy(k.fill, &high.sub(src, n), &mut low.sub(dst, n))
        } else {
            ops::copy(k.fill, &low.sub(src, n), &mut high.sub(dst, n))
        };
        debug_assert!(copied.is_ok(), "both sides are {n} words by construction");
        self.cursor += n;
    }

    /// Bursts of `bus.burst_blocks` alternating between filling one stream and verifying the other.
    fn bus(
        &mut self,
        region: &mut Region<'_>,
        k: Kernels,
        bus: Bus,
        budget: usize,
        now: Instant,
        faults: &mut Faults<'_>,
    ) {
        let burst = bus.burst_blocks * BLOCK_WORDS;
        let (fill_span, check_span) = (self.span, self.check_span);
        let mut spent = 0;
        while spent < budget && self.cursor < self.work {
            let fill = burst.min(fill_span.words - self.bus[0]);
            if fill > 0 {
                let mut chunk = region.sub(fill_span.start + self.bus[0] - self.base, fill);
                ops::fill_prsg(k.fill, &mut chunk, &mut self.prsg, 0);
                self.bus[0] += fill;
            }
            let verify = burst.min(check_span.words - self.bus[1]);
            if verify > 0 {
                let chunk = region.sub(check_span.start + self.bus[1] - self.base, verify);
                ops::verify_prsg(k.verify, &chunk, &mut self.check, 0, faults);
                self.bus[1] += verify;
            }
            spent += fill + verify;
            self.cursor += fill + verify;
            self.since_gap += (fill + verify) * size_of::<u64>();
            if bus.gaps && self.since_gap >= BUS_ON_BYTES && self.cursor < self.work {
                self.since_gap = 0;
                self.until = Some(now + BUS_GAP);
                return;
            }
        }
    }
}

/// The address walk over the largest power-of-two span of the region that is aligned to its size.
#[expect(clippy::needless_pass_by_ref_mut, reason = "the walk writes the region through its pointer")]
fn address_walk(region: &mut Region<'_>, invert: u64, faults: &mut Faults<'_>) {
    let base = region.base().get();
    let end = base + region.bytes();
    let largest = usize::BITS - 1 - region.bytes().max(1).leading_zeros();
    let Some((anchor, bytes)) = (3..=largest).rev().find_map(|bit| {
        let size = 1_usize << bit;
        let anchor = base.next_multiple_of(size);
        (anchor + size <= end).then_some((anchor, size))
    }) else {
        return;
    };
    let anchor = region.ptr().wrapping_byte_add(anchor - base);
    // SAFETY: `anchor..anchor + bytes` lies inside the region.
    unsafe { kernels::address_walk(&mut Dram, anchor, bytes, invert, faults) };
}
