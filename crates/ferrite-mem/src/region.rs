//! Memory under test, as a base pointer and a length rather than a slice.
//!
//! A `&mut [u64]` would let the compiler assume the memory holds what was last stored, which is
//! exactly what a memory test must not assume.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::marker::PhantomData;
use core::ptr::NonNull;

use snafu::Snafu;

use crate::fault::CpuAddr;
use crate::suite::Span;

/// Words in one block, the unit every tier works in: one cache line, one ZMM register.
pub const BLOCK_WORDS: usize = 8;
pub const BLOCK_BYTES: usize = BLOCK_WORDS * size_of::<u64>();
/// Words in a block-move group, the unit a set of regions is trimmed to: two blocks.
pub const GROUP_WORDS: usize = 2 * BLOCK_WORDS;

#[derive(Debug, Snafu, PartialEq, Eq)]
pub enum RegionError {
    #[snafu(display("region base is null"))]
    Null,
    #[snafu(display("region base {addr} is not 64-byte aligned"))]
    Misaligned { addr: CpuAddr },
    #[snafu(display("region of {words} words is not a whole number of 64-byte blocks"))]
    Length { words: usize },
}

/// A 64-byte-aligned range of whole blocks that the holder alone may touch for `'a`.
#[derive(Debug)]
pub struct Region<'a> {
    base: NonNull<u64>,
    words: usize,
    _owns: PhantomData<&'a mut u64>,
}

impl Region<'_> {
    /// # Safety
    /// `base..base + words` must be memory the caller owns exclusively for the region's lifetime: valid for reads and
    /// writes, touched by nothing else (no references, no other region) while the region lives.
    pub unsafe fn from_raw(base: *mut u64, words: usize) -> Result<Self, RegionError> {
        let base = NonNull::new(base).ok_or(RegionError::Null)?;
        if !base.addr().get().is_multiple_of(BLOCK_BYTES) {
            return Err(RegionError::Misaligned { addr: CpuAddr::of(base.as_ptr()) });
        }
        if !words.is_multiple_of(BLOCK_WORDS) {
            return Err(RegionError::Length { words });
        }
        Ok(Self { base, words, _owns: PhantomData })
    }

    #[must_use]
    pub const fn words(&self) -> usize {
        self.words
    }

    #[must_use]
    pub const fn bytes(&self) -> usize {
        self.words * size_of::<u64>()
    }

    #[must_use]
    pub const fn blocks(&self) -> usize {
        self.words / BLOCK_WORDS
    }

    #[must_use]
    pub fn base(&self) -> CpuAddr {
        CpuAddr::of(self.base.as_ptr())
    }

    pub(crate) const fn ptr(&self) -> *mut u64 {
        self.base.as_ptr()
    }

    /// `words` words from `start` (both rounded down to whole blocks), clipped to the region.
    pub fn sub(&mut self, start: usize, words: usize) -> Region<'_> {
        let start = (start - start % BLOCK_WORDS).min(self.words);
        let words = (words - words % BLOCK_WORDS).min(self.words - start);
        // SAFETY: inside this region, block aligned, and borrowed from it for the result's life.
        Region { base: unsafe { self.base.add(start) }, words, _owns: PhantomData }
    }

    /// The region split at word `mid` (rounded down to a block), as two disjoint regions.
    pub fn split_at(&mut self, mid: usize) -> (Region<'_>, Region<'_>) {
        let mid = (mid - mid % BLOCK_WORDS).min(self.words);
        // SAFETY: `mid` is in bounds; the two halves are disjoint and borrow this region.
        let high = unsafe { self.base.add(mid) };
        (
            Region { base: self.base, words: mid, _owns: PhantomData },
            Region { base: high, words: self.words - mid, _owns: PhantomData },
        )
    }

    /// Consecutive chunks of `chunk_words` (rounded down to whole blocks, at least one), the last
    /// one shorter; reversible for descending walks.
    pub fn chunks(&mut self, chunk_words: usize) -> impl DoubleEndedIterator<Item = Region<'_>> + ExactSizeIterator {
        let step = (chunk_words - chunk_words % BLOCK_WORDS).max(BLOCK_WORDS);
        let (base, words) = (self.base, self.words);
        (0..words.div_ceil(step)).map(move |i| {
            let start = i * step;
            // SAFETY: `start < words`; chunks are disjoint and all borrow this region.
            Region { base: unsafe { base.add(start) }, words: step.min(words - start), _owns: PhantomData }
        })
    }
}

impl Region<'_> {
    /// The region cut down to a whole number of `unit`-word groups.
    fn trimmed(mut self, unit: usize) -> Self {
        self.words -= self.words % unit;
        self
    }

    /// The region as a borrow of itself, for as long as it is borrowed.
    fn reborrow(&mut self) -> Region<'_> {
        self.sub(0, self.words)
    }

    /// The region split at word `mid` (rounded down to a block), consuming it.
    fn split_off(self, mid: usize) -> (Self, Self) {
        let mid = (mid - mid % BLOCK_WORDS).min(self.words);
        // SAFETY: `mid` is in bounds; the halves are disjoint and own what this region owned.
        let high = unsafe { self.base.add(mid) };
        (
            Self { base: self.base, words: mid, _owns: PhantomData },
            Self { base: high, words: self.words - mid, _owns: PhantomData },
        )
    }
}

/// Where each region of a set lies in the set's word space: every region begins where the one
/// before it ends. Cheap to clone, so a test can keep the layout it was planned over.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    spans: Arc<[Span]>,
    words: usize,
}

impl Layout {
    /// Regions of `region_words` words each, in order, each cut down as [`Regions::new`] cuts them.
    pub fn new(region_words: impl IntoIterator<Item = usize>) -> Self {
        let mut words = 0;
        let spans: Vec<Span> = region_words
            .into_iter()
            .map(|w| w - w % GROUP_WORDS)
            .filter(|&w| w > 0)
            .map(|w| {
                let span = Span { start: words, words: w };
                words += w;
                span
            })
            .collect();
        Self { spans: spans.into(), words }
    }

    /// Every word of every region.
    #[must_use]
    pub const fn all(&self) -> Span {
        Span { start: 0, words: self.words }
    }

    /// The regions' spans, in order.
    #[must_use]
    pub fn spans(&self) -> &[Span] {
        &self.spans
    }

    #[must_use]
    pub const fn words(&self) -> usize {
        self.words
    }

    #[must_use]
    pub const fn bytes(&self) -> usize {
        self.words * size_of::<u64>()
    }
}

#[derive(Debug)]
struct Part<'a> {
    /// The word of the set this region begins at.
    start: usize,
    region: Region<'a>,
}

impl Part<'_> {
    const fn end(&self) -> usize {
        self.start + self.region.words()
    }
}

/// Memory under test in several disjoint pieces, addressed as one run of words.
///
/// Word 0 is the first word of the first region, and the next region's first word follows the last
/// word of the one before. The tests plan over that run, so their sequences (pseudo-random
/// streams, shifting patterns, strides) continue across the gaps between regions, while every
/// access stays inside one.
#[derive(Debug)]
pub struct Regions<'a> {
    parts: Vec<Part<'a>>,
    words: usize,
}

impl<'a> Regions<'a> {
    /// The set of `regions`, in the order given, each cut down to a whole number of block-move
    /// groups (128 bytes) and dropped if nothing is left. Page-aligned memory loses nothing.
    pub fn new(regions: impl IntoIterator<Item = Region<'a>>) -> Self {
        let mut parts = Vec::new();
        let mut words = 0;
        for region in regions {
            let region = region.trimmed(GROUP_WORDS);
            if region.words() > 0 {
                let start = words;
                words += region.words();
                parts.push(Part { start, region });
            }
        }
        Self { parts, words }
    }

    #[must_use]
    pub const fn words(&self) -> usize {
        self.words
    }

    #[must_use]
    pub const fn bytes(&self) -> usize {
        self.words * size_of::<u64>()
    }

    #[must_use]
    pub fn layout(&self) -> Layout {
        let spans: Vec<Span> = self.parts.iter().map(|p| Span { start: p.start, words: p.region.words() }).collect();
        Layout { spans: spans.into(), words: self.words }
    }

    /// Words `start..start + words` (rounded down to whole blocks, clipped to the set) as the
    /// pieces of them in each region they touch, each with the word of the set it begins at.
    pub fn sub(&mut self, start: usize, words: usize) -> impl DoubleEndedIterator<Item = (usize, Region<'_>)> {
        let end = start.saturating_add(words).min(self.words);
        let start = start.min(end);
        let first = self.parts.partition_point(|p| p.end() <= start);
        let last = self.parts.partition_point(|p| p.start < end).max(first);
        self.parts[first..last].iter_mut().map(move |p| {
            let (from, to) = (start.max(p.start), end.min(p.end()));
            (from, p.region.sub(from - p.start, to - from))
        })
    }

    /// The words of each of `spans` (ascending and disjoint, on block boundaries) as a set of its
    /// own, whose word 0 is the span's first. Words in no span belong to no set.
    pub fn split(&mut self, spans: &[Span]) -> Vec<Regions<'_>> {
        let mut pieces: VecDeque<(usize, Region<'_>)> =
            self.parts.iter_mut().map(|p| (p.start, p.region.reborrow())).collect();
        spans
            .iter()
            .map(|&span| {
                let mut parts = Vec::new();
                while let Some((start, region)) = pieces.pop_front() {
                    let end = start + region.words();
                    if end <= span.start || region.words() == 0 {
                        continue;
                    }
                    if start >= span.end() {
                        pieces.push_front((start, region));
                        break;
                    }
                    let (start, region) = if start < span.start {
                        (span.start, region.split_off(span.start - start).1)
                    } else {
                        (start, region)
                    };
                    if end > span.end() {
                        let (head, tail) = region.split_off(span.end() - start);
                        pieces.push_front((span.end(), tail));
                        parts.push(Part { start: start - span.start, region: head });
                        break;
                    }
                    parts.push(Part { start: start - span.start, region });
                }
                let words = parts.iter().map(|p| p.region.words()).sum();
                Regions { parts, words }
            })
            .collect()
    }
}

/// Memory a test steps over as one run of words: a [`Region`], or a set of them ([`Regions`]).
pub trait Memory {
    /// Where the regions lie in the run.
    fn layout(&self) -> Layout;

    fn words(&self) -> usize;

    /// Words `start..start + words` (rounded down to whole blocks, clipped) as the piece of them in
    /// each region they touch, each with the word of the run it begins at.
    fn pieces(&mut self, start: usize, words: usize) -> impl DoubleEndedIterator<Item = (usize, Region<'_>)>;
}

impl Memory for Region<'_> {
    fn layout(&self) -> Layout {
        Layout::new([self.words])
    }

    fn words(&self) -> usize {
        self.words
    }

    fn pieces(&mut self, start: usize, words: usize) -> impl DoubleEndedIterator<Item = (usize, Region<'_>)> {
        let start = (start - start % BLOCK_WORDS).min(self.words);
        core::iter::once((start, self.sub(start, words)))
    }
}

impl Memory for Regions<'_> {
    fn layout(&self) -> Layout {
        Regions::layout(self)
    }

    fn words(&self) -> usize {
        self.words
    }

    fn pieces(&mut self, start: usize, words: usize) -> impl DoubleEndedIterator<Item = (usize, Region<'_>)> {
        self.sub(start, words)
    }
}

impl<'a> From<Region<'a>> for Regions<'a> {
    fn from(region: Region<'a>) -> Self {
        Self::new([region])
    }
}

#[cfg(test)]
impl Region<'_> {
    /// Word `word`, read past the kernels, for tests that inject faults while a test holds the region.
    pub(crate) fn peek(&self, word: usize) -> u64 {
        assert!(word < self.words);
        // SAFETY: in bounds, and the region is ours.
        unsafe { self.base.add(word).read() }
    }

    pub(crate) fn poke(&mut self, word: usize, value: u64) {
        assert!(word < self.words);
        // SAFETY: as `peek`.
        unsafe { self.base.add(word).write(value) };
    }

    pub(crate) fn addr(&self, word: usize) -> CpuAddr {
        CpuAddr::of(self.base.as_ptr().wrapping_add(word))
    }
}

#[cfg(test)]
impl Regions<'_> {
    /// Word `word` of the set, read past the kernels, as [`Region::peek`].
    pub(crate) fn peek(&self, word: usize) -> u64 {
        let part = self.part_of(word);
        part.region.peek(word - part.start)
    }

    pub(crate) fn poke(&mut self, word: usize, value: u64) {
        let i = self.parts.partition_point(|p| p.end() <= word);
        let part = &mut self.parts[i];
        part.region.poke(word - part.start, value);
    }

    pub(crate) fn addr(&self, word: usize) -> CpuAddr {
        let part = self.part_of(word);
        part.region.addr(word - part.start)
    }

    fn part_of(&self, word: usize) -> &Part<'_> {
        &self.parts[self.parts.partition_point(|p| p.end() <= word)]
    }
}

#[cfg(test)]
mod tests;
