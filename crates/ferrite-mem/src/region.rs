//! Memory under test, as a base pointer and a length rather than a slice.
//!
//! A `&mut [u64]` would let the compiler assume the memory holds what was last stored, which is
//! exactly what a memory test must not assume.

use core::marker::PhantomData;
use core::ptr::NonNull;

use snafu::Snafu;

use crate::fault::CpuAddr;

/// Words in one block, the unit every tier works in: one cache line, one ZMM register.
pub const BLOCK_WORDS: usize = 8;
pub const BLOCK_BYTES: usize = BLOCK_WORDS * size_of::<u64>();

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
