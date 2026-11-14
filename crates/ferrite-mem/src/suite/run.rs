//! Running a test's phases chunk by chunk.

use core::time::Duration;

use ferrite_hal::Instant;

use super::plan::{self, Bus, MODULO, Op, Phase, Seq, Span, move_half};
use super::{Config, Kernels, TestKind, percent};
use crate::fault::Faults;
use crate::kernels::{self, Dram, Stride};
use crate::ops::{self, Order, Source};
use crate::prsg::Prsg;
use crate::region::{BLOCK_WORDS, GROUP_WORDS, Layout, Memory, Region};
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

/// One test over a set of regions, or over one core's share of it, as a sequence of phases advanced
/// by [`Test::step`].
#[derive(Debug)]
pub struct Test {
    kind: TestKind,
    config: Config,
    pass: u64,
    /// The regions the phases are planned over.
    layout: Layout,
    /// The words of them this test works on.
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
    /// The test for pass `pass` over `memory`; every step must get the same memory.
    #[must_use]
    pub fn new(kind: TestKind, config: Config, pass: u64, memory: &impl Memory) -> Self {
        let layout = memory.layout();
        Self::for_share(kind, config, pass, &layout, Span { start: 0, words: layout.words() })
    }

    /// The test planned over the regions of `layout`, working only on `share` of their words: every
    /// step must get the regions of `share` alone, whose word 0 is word `share.start` of the whole.
    /// The sequences (PRSG, shifting pattern, stride, move pattern) continue across shares, so the
    /// shares of one set together do what the whole-set test does.
    #[must_use]
    pub fn for_share(kind: TestKind, config: Config, pass: u64, layout: &Layout, share: Span) -> Self {
        let m = plan::measure(kind, &config, pass, layout, share);
        Self {
            kind,
            config,
            pass,
            layout: layout.clone(),
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
    /// before. Consecutive block-move windows go without, since each lies inside one share, and so
    /// do the windows of own address in windows, which each core fills and checks over its own part
    /// of the window.
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
    pub fn step<M: Memory + ?Sized>(
        &mut self,
        memory: &mut M,
        kernels: &Kernels,
        budget_bytes: usize,
        now: Instant,
        faults: &mut Faults<'_>,
    ) -> Progress {
        debug_assert!(memory.words() == self.share.words, "a step gets its share's memory");
        if self.phase >= self.phases {
            return Progress::Done;
        }
        if self.active.is_none() {
            let next = self.next.take();
            let Some(phase) = next.or_else(|| plan::nth(self.kind, &self.config, self.pass, &self.layout, self.phase))
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
            && let Some(until) = active.run(memory, kernels, budget, now, faults)
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
        self.next = plan::nth(self.kind, &self.config, self.pass, &self.layout, self.phase);
        self.gated = !(self.kind == TestKind::OwnAddressWindow
            || windowed && self.next.is_some_and(|p| matches!(p.op, Op::Move { .. })));
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
    seqs: Seqs,
    /// The bus-stress verify stream.
    check: Prsg,
    /// Words done of the bus-stress fill and verify streams.
    bus: [usize; 2],
    since_gap: usize,
    until: Option<Instant>,
}

/// The sequence states a phase carries from chunk to chunk.
#[derive(Debug)]
struct Seqs {
    prsg: Prsg,
    rot: Rotating,
    /// The sequences a replace pass checks and writes.
    replace: [Walker; 2],
}

#[derive(Debug)]
enum Walker {
    Word(u64),
    Prsg(Prsg),
    Rotating(Rotating),
}

impl Walker {
    fn at(seq: Seq, block: u64) -> Self {
        match seq {
            Seq::Word(word) => Self::Word(word),
            Seq::Prsg { seed } => Self::Prsg(Prsg::at(seed, block)),
            Seq::Rotating { pattern } => Self::Rotating(Rotating::at(pattern, block)),
        }
    }

    fn source(&mut self) -> Source<'_> {
        match self {
            Self::Word(word) => Source::Word(*word),
            Self::Prsg(prsg) => Source::Prsg(prsg),
            Self::Rotating(rot) => Source::Rotating(rot),
        }
    }
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
        let mut seqs =
            Seqs { prsg: Prsg::new(0), rot: Rotating::at(0, 0), replace: [Walker::Word(0), Walker::Word(0)] };
        let mut check = Prsg::new(0);
        match phase.op {
            Op::FillPrsg { seed, .. } | Op::VerifyPrsg { seed, .. } | Op::InvertPrsg { seed, .. } => {
                seqs.prsg = Prsg::at(seed, entry(span, phase.span));
            },
            Op::FillRotating { pattern, .. } | Op::InvertRotating { pattern, .. } => {
                seqs.rot = Rotating::at(pattern, entry(span, phase.span));
            },
            Op::Replace { from, to, .. } => {
                let block = entry(span, phase.span);
                seqs.replace = [Walker::at(from, block), Walker::at(to, block)];
            },
            Op::Bus(bus) => {
                span = bus.fill.span.clip(share);
                seqs.prsg = Prsg::at(bus.fill.seed, entry(span, bus.fill.span));
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
            seqs,
            check,
            bus: [0; 2],
            since_gap: 0,
            until: None,
        }
    }

    /// Run one chunk of at most `budget` words, or return the time to wait until.
    fn run<M: Memory + ?Sized>(
        &mut self,
        regions: &mut M,
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
                let span = self.span;
                for (_, mut region) in regions.pieces(span.start - self.base, span.words) {
                    address_walk(&mut region, invert, faults);
                }
                self.cursor = self.work;
            },
            Op::Move { .. } => self.block_move(regions, *k, budget),
            Op::Bus(bus) => self.bus(regions, *k, bus, budget, now, faults),
            Op::MoveInit => {
                let (start, n) = self.next_chunk(budget.next_multiple_of(GROUP_WORDS));
                for (at, chunk) in regions.pieces(start - self.base, n) {
                    // SAFETY: the chunk is a region, so its blocks are ours.
                    unsafe { kernels::fill_move_pattern(chunk.ptr(), chunk.blocks(), (self.base + at) / GROUP_WORDS) };
                }
            },
            _ => self.linear_op(regions, *k, budget, faults),
        }
        None
    }

    /// The next chunk of a phase that walks its span once, as its first word of the whole set
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

    fn linear_op<M: Memory + ?Sized>(&mut self, regions: &mut M, k: Kernels, budget: usize, faults: &mut Faults<'_>) {
        let op = self.phase.op;
        let (start, n) = self.next_chunk(budget);
        let pieces = regions.pieces(start - self.base, n);
        let base = self.base;
        let seqs = &mut self.seqs;
        let mut chunk = |at: usize, mut c: Region<'_>| linear_chunk(op, k, &mut c, base + at, seqs, faults);
        if self.phase.order() == Order::Descending {
            pieces.rev().for_each(|(at, c)| chunk(at, c));
        } else {
            pieces.for_each(|(at, c)| chunk(at, c));
        }
    }

    /// memtest86+'s block move over one window, `iterations` times: the low half is copied onto the
    /// high half, the high half less its last block onto the low half one block up, and that last
    /// block onto the start. Each copy is ascending, so it can be split into chunks.
    fn block_move<M: Memory + ?Sized>(&mut self, regions: &mut M, k: Kernels, budget: usize) {
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
        // A window lies inside one region, so it is one piece.
        if let Some((_, mut window)) = regions.pieces(span.start - self.base, 2 * half).next() {
            let (mut low, mut high) = window.split_at(half);
            let copied = if from_high {
                ops::copy(k.fill, &high.sub(src, n), &mut low.sub(dst, n))
            } else {
                ops::copy(k.fill, &low.sub(src, n), &mut high.sub(dst, n))
            };
            debug_assert!(copied.is_ok(), "both sides are {n} words by construction");
        }
        self.cursor += n;
    }

    /// Bursts of `bus.burst_blocks` alternating between filling one stream and verifying the other.
    fn bus<M: Memory + ?Sized>(
        &mut self,
        regions: &mut M,
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
                for (_, mut chunk) in regions.pieces(fill_span.start + self.bus[0] - self.base, fill) {
                    ops::fill_prsg(k.fill, &mut chunk, &mut self.seqs.prsg, 0);
                }
                self.bus[0] += fill;
            }
            let verify = burst.min(check_span.words - self.bus[1]);
            if verify > 0 {
                for (_, chunk) in regions.pieces(check_span.start + self.bus[1] - self.base, verify) {
                    ops::verify_prsg(k.verify, &chunk, &mut self.check, 0, faults);
                }
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

/// One piece of a linear phase's chunk, `start` being its first word of the whole set.
fn linear_chunk(op: Op, k: Kernels, c: &mut Region<'_>, start: usize, seqs: &mut Seqs, faults: &mut Faults<'_>) {
    let Seqs { prsg, rot, replace: [from, to] } = seqs;
    let stride = |offset| Stride { n: MODULO, offset, first: start % MODULO };
    match op {
        Op::Fill(v) => ops::fill(k.fill, c, v),
        Op::Verify(v) => ops::verify(k.verify, c, v, faults),
        Op::Invert(step) => ops::invert(k.rmw, c, step, faults),
        Op::FillAddress(key) => ops::fill_address(k.fill, c, key),
        Op::VerifyAddress(key) => ops::verify_address(k.verify, c, key, faults),
        Op::FillPrsg { key, .. } => ops::fill_prsg(k.fill, c, prsg, key),
        Op::VerifyPrsg { key, .. } => ops::verify_prsg(k.verify, c, prsg, key, faults),
        Op::InvertPrsg { step, .. } => ops::invert_prsg(k.rmw, c, prsg, step, faults),
        Op::FillRotating { key, .. } => ops::fill_rotating(k.fill, c, rot, key),
        Op::InvertRotating { step, .. } => ops::invert_rotating(k.rmw, c, rot, step, faults),
        Op::Replace { step, .. } => ops::replace(k.rmw, c, from.source(), to.source(), step, faults),
        Op::FillExcept { offset, value } => ops::fill_except(k.fill, c, stride(offset), value),
        // SAFETY: the chunk is a region, so its words are ours.
        Op::FillStride { offset, value } => unsafe {
            kernels::fill_stride(c.ptr(), c.words(), stride(offset), value);
        },
        Op::VerifyStride { offset, value } => {
            // SAFETY: as above.
            unsafe { kernels::verify_stride(c.ptr(), c.words(), stride(offset), value, faults) };
        },
        Op::VerifyFillStride { check_offset, expect, offset, value } => {
            // SAFETY: as above.
            unsafe {
                kernels::verify_fill_stride(
                    c.ptr(),
                    c.words(),
                    stride(check_offset),
                    expect,
                    stride(offset),
                    value,
                    faults,
                );
            };
        },
        // SAFETY: as above.
        Op::VerifyPairs => unsafe { kernels::verify_pairs(c.ptr(), c.blocks(), faults) },
        Op::MoveInit | Op::Move { .. } | Op::AddressWalk { .. } | Op::Bus(_) | Op::Wait(_) => {},
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
