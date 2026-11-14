//! Which tests split over several cores, and how a region is cut into the cores' shares.
//!
//! A split test runs [`Test::for_share`](super::Test::for_share) on every share at once, and the
//! cores meet at each gated phase boundary ([`Test::gated`](super::Test::gated)), as memtest86+'s
//! parallel mode does with its barriers. Every word then gets the same values and checks as in the
//! whole-set run, since the sequences continue across shares; what changes is the order in
//! which words of different shares are touched within a phase.

use core::num::NonZeroUsize;

use alloc::vec;
use alloc::vec::Vec;

use super::plan::{Span, move_windows};
use super::{Config, TestKind};
use crate::region::{Layout, Regions};

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
    /// Only the address walk stays on one core: its coverage depends on the one aligned span it
    /// walks, and it does a few hundred accesses a region.
    ///
    /// Every other test splits, with its sequences continued across shares, block-move windows
    /// kept whole and bus stress keeping its two halves of the whole region (each core fills and
    /// checks the parts of them in its share). Own address fills every share, meets at the gate,
    /// then every core checks its share, so an address that aliases one in another share is caught
    /// as in the whole run: whichever write lands last, one of the two cells disagrees. March C-
    /// meets at a gate between its elements and each core sweeps its share in the element's
    /// direction. A cell and a far-away one in another share are then touched concurrently within
    /// an element rather than in address order, so a coupling or decoder fault across a share
    /// edge is no longer caught deterministically by this test (own address still covers
    /// aliasing); faults inside a share, and between neighbours, are unaffected. Own address in
    /// windows needs no meeting at all: each core fills and checks its part of every window in turn.
    /// A window that straddles two shares is then checked in two parts, so an alias between the
    /// parts is no longer caught, which touches only the few windows at a cut.
    #[must_use]
    pub const fn mode(self) -> Mode {
        match self {
            Self::AddressWalk => Mode::Single,
            _ => Mode::Split,
        }
    }
}

/// `regions` cut into the consecutive shares of `kind` for at most `parts` cores, each with its span
/// of the whole set.
///
/// A single-core test gets one share of everything. Otherwise the shares are cut between block-move
/// windows (see [`move_windows`]) so that none spans two, and the cuts are the same for every
/// split test, so a run of them can keep each core on its own memory throughout. Each share takes
/// about its fair share of what is left of the words, so shares differ by less than a window.
pub fn shares<'r>(
    regions: &'r mut Regions<'_>,
    kind: TestKind,
    cfg: &Config,
    parts: NonZeroUsize,
) -> impl ExactSizeIterator<Item = (Span, Regions<'r>)> {
    let spans = share_spans(&regions.layout(), kind, cfg, parts);
    let views = regions.split(&spans);
    spans.into_iter().zip(views)
}

/// The spans of `kind`'s shares over `layout` for at most `parts` cores.
pub(super) fn share_spans(layout: &Layout, kind: TestKind, cfg: &Config, parts: NonZeroUsize) -> Vec<Span> {
    let words = layout.words();
    if kind.mode() == Mode::Single {
        return vec![Span { start: 0, words }];
    }
    let mut spans: Vec<Span> = Vec::new();
    let mut start = 0;
    for window in move_windows(layout, cfg) {
        let left = parts.get() - spans.len();
        let target = (words - start).div_ceil(left);
        if left > 1 && window.start - start >= target {
            spans.push(Span { start, words: window.start - start });
            start = window.start;
        }
    }
    if start < words {
        spans.push(Span { start, words: words - start });
    }
    spans
}
