//! Which tests split over several cores, and how a region is cut into the cores' shares.
//!
//! A split test runs [`Test::for_share`](super::Test::for_share) on every share at once, and the
//! cores meet at each gated phase boundary ([`Test::gated`](super::Test::gated)), as memtest86+'s
//! parallel mode does with its barriers. Every word then gets the same values and checks as in the
//! whole-region run, since the sequences continue across shares; what changes is the order in
//! which words of different shares are touched within a phase.

use core::num::NonZeroUsize;

use super::plan::{Span, move_window};
use super::{Config, TestKind};
use crate::region::{BLOCK_WORDS, Region};

/// How a test runs when several cores are available.
#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::IntoStaticStr)]
#[strum(serialize_all = "kebab-case")]
pub enum Mode {
    /// On one core over the whole region.
    Single,
    /// On every core at once, each over its own share.
    Split,
}

impl TestKind {
    /// How the test runs when several cores are available.
    ///
    /// The address tests and March C- stay on one core: the faults they target (a decoder that
    /// maps one address onto another, a cell disturbed by one far away) show only through the
    /// order of accesses across the whole region, which split shares would interleave. The
    /// address walk's coverage also depends on the span it walks. They are quick, so this costs
    /// about a second a pass. Every other test splits, with its sequences continued across
    /// shares, block-move windows kept whole and bus stress keeping its two halves of the whole
    /// region (each core fills and checks the parts of them in its share).
    #[must_use]
    pub const fn mode(self) -> Mode {
        match self {
            Self::AddressWalk | Self::OwnAddress | Self::OwnAddressWindow | Self::MarchCMinus => Mode::Single,
            _ => Mode::Split,
        }
    }
}

/// The words every share of `kind` but the last is a whole number of: a block, or for the block
/// move one window, so no window spans two shares.
fn unit(kind: TestKind, cfg: &Config) -> usize {
    match kind {
        TestKind::BlockMove => move_window(cfg),
        _ => BLOCK_WORDS,
    }
}

/// Words in each share of `kind` over `words` words for at most `parts` cores; the last share may
/// be shorter, and a single-core test gets one share of everything.
fn share_words(kind: TestKind, cfg: &Config, words: usize, parts: NonZeroUsize) -> usize {
    let parts = if kind.mode() == Mode::Single { 1 } else { parts.get() };
    let unit = unit(kind, cfg);
    words.div_ceil(unit).div_ceil(parts).max(1) * unit
}

/// `region` cut into the consecutive shares of `kind` for at most `parts` cores, each with its span
/// in the whole region.
pub fn shares<'r>(
    region: &'r mut Region<'_>,
    kind: TestKind,
    cfg: &Config,
    parts: NonZeroUsize,
) -> impl ExactSizeIterator<Item = (Span, Region<'r>)> {
    let step = share_words(kind, cfg, region.words(), parts);
    region.chunks(step).enumerate().map(move |(i, r)| (Span { start: i * step, words: r.words() }, r))
}
