//! Each test as its list of phases.

use core::ops::ControlFlow;
use core::time::Duration;

use super::{Config, TestKind};
use crate::ops::{Inversion, Order};
use crate::prsg::{splitmix, xorshift};
use crate::region::BLOCK_WORDS;

/// The modulo test's stride.
pub const MODULO: usize = 20;
/// A move window is a whole number of block-move groups (16 words, two blocks).
const GROUP_WORDS: usize = 2 * BLOCK_WORDS;

/// Words `start..start + words` of the region, block aligned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub words: usize,
}

impl Span {
    pub const fn end(self) -> usize {
        self.start + self.words
    }

    pub const fn blocks(self) -> u64 {
        (self.words / BLOCK_WORDS) as u64
    }
}

/// A PRSG sequence starting at its span's first word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stream {
    pub span: Span,
    pub seed: u64,
}

/// One bus-stress sweep: bursts filling one half alternate with bursts verifying the other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bus {
    pub fill: Stream,
    pub check: Option<Stream>,
    pub burst_blocks: usize,
    /// Pause after every few MiB, for load steps in the module's power delivery.
    pub gaps: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Fill(u64),
    Verify(u64),
    Invert(Inversion),
    FillAddress(u64),
    VerifyAddress(u64),
    FillPrsg {
        seed: u64,
        key: u64,
    },
    VerifyPrsg {
        seed: u64,
        key: u64,
    },
    InvertPrsg {
        seed: u64,
        step: Inversion,
    },
    FillRotating {
        pattern: u64,
        key: u64,
    },
    InvertRotating {
        pattern: u64,
        step: Inversion,
    },
    /// Write `value` to every [`MODULO`]-th word, the ones at `offset`.
    FillStride {
        offset: usize,
        value: u64,
    },
    FillExcept {
        offset: usize,
        value: u64,
    },
    VerifyStride {
        offset: usize,
        value: u64,
    },
    MoveInit,
    /// The block moves of one window, `iterations` times over.
    Move {
        iterations: u32,
    },
    VerifyPairs,
    AddressWalk {
        invert: u64,
    },
    Bus(Bus),
    Wait(Duration),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Phase {
    pub op: Op,
    pub span: Span,
}

impl Phase {
    /// Words of work, the unit of step budgets and of progress.
    pub const fn work(&self) -> usize {
        match self.op {
            Op::AddressWalk { .. } => BLOCK_WORDS,
            Op::Move { iterations } => iterations as usize * 2 * move_half(self.span),
            Op::Bus(bus) => bus.fill.span.words + if let Some(c) = bus.check { c.span.words } else { 0 },
            _ => self.span.words,
        }
    }

    pub const fn order(&self) -> Order {
        match self.op {
            Op::Invert(step) | Op::InvertPrsg { step, .. } | Op::InvertRotating { step, .. } => step.order,
            _ => Order::Ascending,
        }
    }
}

/// Words in each half of a move window; the two halves cover whole blocks of it.
pub const fn move_half(span: Span) -> usize {
    span.words / GROUP_WORDS * BLOCK_WORDS
}

/// The phases of `kind` over a region of `words` words, in order, until `emit` breaks.
#[expect(clippy::too_many_lines, reason = "one arm per test, each a few lines")]
pub fn plan(
    kind: TestKind,
    cfg: &Config,
    pass: u64,
    words: usize,
    emit: &mut dyn FnMut(Phase) -> ControlFlow<()>,
) -> ControlFlow<()> {
    let mut e = Emit { emit, all: Span { start: 0, words } };
    let seed = |round: u64| mix(cfg.seed, kind as u64, pass, round);
    match kind {
        TestKind::AddressWalk => {
            for invert in [0, !0] {
                e.whole(Op::AddressWalk { invert })?;
            }
        },
        TestKind::OwnAddress => {
            for key in [0, !0] {
                e.whole(Op::FillAddress(key))?;
                e.whole(Op::VerifyAddress(key))?;
            }
        },
        TestKind::OwnAddressWindow => {
            for span in windows(words, (cfg.address_window_bytes / 8).max(BLOCK_WORDS), BLOCK_WORDS) {
                e.on(span, Op::FillAddress(0))?;
                e.on(span, Op::VerifyAddress(0))?;
            }
        },
        TestKind::SolidBits => solid(&mut e, [0, !0])?,
        TestKind::Checkerboard => solid(&mut e, [0xaaaa_aaaa_aaaa_aaaa, 0x5555_5555_5555_5555])?,
        TestKind::WalkingOnes => solid(&mut e, (0..64).map(|bit| 1 << bit))?,
        TestKind::WalkingZeros => solid(&mut e, (0..64).map(|bit| !(1 << bit)))?,
        TestKind::MarchCMinus => {
            e.whole(Op::Fill(0))?;
            for (order, from) in
                [(Order::Ascending, 0), (Order::Ascending, !0), (Order::Descending, 0), (Order::Descending, !0)]
            {
                e.whole(Op::Invert(Inversion { expect: from, write: !from, order }))?;
            }
            e.whole(Op::Verify(0))?;
        },
        TestKind::MovingInversions => {
            for p in [0, !0] {
                fixed(&mut e, p, cfg.fixed_iterations)?;
            }
        },
        TestKind::MovingInversions8Bit => {
            for shift in 0..8 {
                let p = 0x8080_8080_8080_8080_u64 >> shift;
                for p in [p, !p] {
                    fixed(&mut e, p, cfg.byte_iterations)?;
                }
            }
        },
        TestKind::MovingInversionsRandom => {
            for round in 0..u64::from(cfg.random_rounds) {
                let seed = seed(round);
                if round % 4 == 3 {
                    fixed(&mut e, xorshift(seed | 1), 1)?;
                } else {
                    e.whole(Op::FillPrsg { seed, key: 0 })?;
                    e.whole(Op::InvertPrsg {
                        seed,
                        step: Inversion { expect: 0, write: !0, order: Order::Ascending },
                    })?;
                    e.whole(Op::InvertPrsg {
                        seed,
                        step: Inversion { expect: !0, write: 0, order: Order::Descending },
                    })?;
                }
            }
        },
        TestKind::MovingInversions64Bit => {
            for offset in 0..64 {
                let pattern = 1 << offset;
                for key in [0, !0] {
                    e.whole(Op::FillRotating { pattern, key })?;
                    for _ in 0..cfg.shifting_iterations {
                        let up = Inversion { expect: key, write: !key, order: Order::Ascending };
                        e.whole(Op::InvertRotating { pattern, step: up })?;
                        let down = Inversion { expect: !key, write: key, order: Order::Descending };
                        e.whole(Op::InvertRotating { pattern, step: down })?;
                    }
                }
            }
        },
        TestKind::Modulo20 => {
            let mut state = seed(0) | 1;
            for _ in 0..cfg.modulo_rounds {
                for offset in 0..MODULO {
                    state = xorshift(state);
                    for value in [state, !state] {
                        e.whole(Op::FillStride { offset, value })?;
                        for _ in 0..2 {
                            e.whole(Op::FillExcept { offset, value: !value })?;
                        }
                        e.whole(Op::VerifyStride { offset, value })?;
                    }
                }
            }
        },
        TestKind::BlockMove => {
            e.whole(Op::MoveInit)?;
            let window = (cfg.move_window_bytes / 8).max(GROUP_WORDS);
            for span in windows(words, window, GROUP_WORDS) {
                e.on(span, Op::Move { iterations: cfg.move_iterations })?;
            }
            e.whole(Op::VerifyPairs)?;
        },
        TestKind::BusStress => {
            let blocks = words / BLOCK_WORDS;
            let low = Span { start: 0, words: blocks / 2 * BLOCK_WORDS };
            let high = Span { start: low.words, words: words - low.words };
            let stream = |span, i: u64, half: u64| Stream { span, seed: seed(2 * i + half) };
            for i in 0..u64::from(cfg.bus_iterations) {
                // memtest86+ bursts 8 << (i % 3) blocks of 32 bytes.
                let burst_blocks = 4 << (i % 3);
                let gaps = i % 2 == 1;
                let check = i.checked_sub(1).map(|prev| stream(high, prev, 1));
                e.whole(Op::Bus(Bus { fill: stream(low, i, 0), check, burst_blocks, gaps }))?;
                let check = Some(stream(low, i, 0));
                e.whole(Op::Bus(Bus { fill: stream(high, i, 1), check, burst_blocks, gaps }))?;
            }
            if let Some(last) = u64::from(cfg.bus_iterations).checked_sub(1) {
                let Stream { span, seed } = stream(high, last, 1);
                e.on(span, Op::VerifyPrsg { seed, key: 0 })?;
            }
        },
        TestKind::BitFade => {
            let seed = seed(0);
            let rounds = [
                (Op::Fill(0), Op::Verify(0)),
                (Op::Fill(!0), Op::Verify(!0)),
                (Op::FillPrsg { seed, key: 0 }, Op::VerifyPrsg { seed, key: 0 }),
                (Op::FillPrsg { seed, key: !0 }, Op::VerifyPrsg { seed, key: !0 }),
            ];
            for (fill, verify) in rounds {
                e.whole(fill)?;
                e.whole(Op::Wait(cfg.fade))?;
                e.whole(verify)?;
            }
        },
    }
    ControlFlow::Continue(())
}

struct Emit<'e> {
    emit: &'e mut dyn FnMut(Phase) -> ControlFlow<()>,
    all: Span,
}

impl Emit<'_> {
    fn on(&mut self, span: Span, op: Op) -> ControlFlow<()> {
        (self.emit)(Phase { op, span })
    }

    fn whole(&mut self, op: Op) -> ControlFlow<()> {
        self.on(self.all, op)
    }
}

/// Fill and verify with each value.
fn solid(e: &mut Emit<'_>, values: impl IntoIterator<Item = u64>) -> ControlFlow<()> {
    for v in values {
        e.whole(Op::Fill(v))?;
        e.whole(Op::Verify(v))?;
    }
    ControlFlow::Continue(())
}

/// memtest86+'s fixed-pattern moving inversions: fill `p`, then up checking `p` writing `!p` and down
/// checking `!p` writing `p`, `iterations` times.
fn fixed(e: &mut Emit<'_>, p: u64, iterations: u32) -> ControlFlow<()> {
    e.whole(Op::Fill(p))?;
    for _ in 0..iterations {
        e.whole(Op::Invert(Inversion { expect: p, write: !p, order: Order::Ascending }))?;
        e.whole(Op::Invert(Inversion { expect: !p, write: p, order: Order::Descending }))?;
    }
    ControlFlow::Continue(())
}

/// Consecutive windows of `window` words (rounded down to `unit`), the last one shorter.
fn windows(words: usize, window: usize, unit: usize) -> impl Iterator<Item = Span> {
    let window = (window - window % unit).max(unit);
    (0..words).step_by(window).map(move |start| Span { start, words: window.min(words - start) })
}

/// A seed for one round of one test in one pass.
fn mix(seed: u64, kind: u64, pass: u64, round: u64) -> u64 {
    [kind, pass, round]
        .into_iter()
        .fold(seed, |s, x| splitmix(s ^ x.wrapping_add(1).wrapping_mul(0x9e37_79b9_7f4a_7c15)))
}

/// The number of phases and their total work.
pub fn measure(kind: TestKind, cfg: &Config, pass: u64, words: usize) -> (usize, u64) {
    let (mut count, mut work) = (0, 0_u64);
    let _ = plan(kind, cfg, pass, words, &mut |phase| {
        count += 1;
        work += phase.work() as u64;
        ControlFlow::Continue(())
    });
    (count, work)
}

/// Phase `index`, if the test has that many.
pub fn nth(kind: TestKind, cfg: &Config, pass: u64, words: usize, index: usize) -> Option<Phase> {
    let mut seen = 0;
    let mut found = None;
    let _ = plan(kind, cfg, pass, words, &mut |phase| {
        if seen == index {
            found = Some(phase);
            return ControlFlow::Break(());
        }
        seen += 1;
        ControlFlow::Continue(())
    });
    found
}
