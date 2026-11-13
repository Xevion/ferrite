//! The shifting pattern: one word rotated a bit further left at every word of the region.

use crate::region::BLOCK_WORDS;

/// Lane states positioned at one block of the sequence `pattern.rotate_left(w % 64)` for word `w`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rotating {
    pub(crate) lanes: [u64; BLOCK_WORDS],
}

impl Rotating {
    /// The sequence for `pattern`, at `block`.
    #[must_use]
    pub fn at(pattern: u64, block: u64) -> Self {
        Self { lanes: core::array::from_fn(|i| Self::word(pattern, block * BLOCK_WORDS as u64 + i as u64)) }
    }

    /// The `word`-th word of the sequence for `pattern`.
    #[must_use]
    pub const fn word(pattern: u64, word: u64) -> u64 {
        pattern.rotate_left((word % 64) as u32)
    }
}
