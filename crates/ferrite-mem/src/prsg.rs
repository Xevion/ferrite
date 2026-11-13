//! The pseudo-random sequence: eight xorshift64 lanes, one per word of a 64-byte block.
//!
//! The word at index `w` of a sequence seeded with `seed` (counted from where the sequence starts,
//! usually the region base) is `xorshift^(w / 8)(lane[w % 8])`, where xorshift is
//! `x ^= x << 13; x ^= x >> 7; x ^= x << 17` and `lane[i]` is the `(i + 1)`-th `SplitMix64` output for
//! `seed` (`z = seed + (i + 1) * 0x9e3779b97f4a7c15`, then the `SplitMix64` finaliser), with 0 replaced
//! by `0x9e3779b97f4a7c15` since 0 is xorshift's fixed point. Fills XOR each word with a key.

use crate::region::BLOCK_WORDS;

const GOLDEN: u64 = 0x9e37_79b9_7f4a_7c15;

/// Lane states positioned at one block of the sequence; fills and verifies advance it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prsg {
    pub(crate) lanes: [u64; BLOCK_WORDS],
}

impl Prsg {
    /// The sequence for `seed`, at block 0.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        let lane = |i: u64| match splitmix(seed.wrapping_add((i + 1).wrapping_mul(GOLDEN))) {
            0 => GOLDEN,
            z => z,
        };
        Self { lanes: core::array::from_fn(|i| lane(i as u64)) }
    }

    /// The sequence for `seed`, at `block`, without stepping through the blocks before it.
    #[must_use]
    pub fn at(seed: u64, block: u64) -> Self {
        let mut prsg = Self::new(seed);
        prsg.skip(block);
        prsg
    }

    /// Move `blocks` blocks forward.
    pub fn skip(&mut self, blocks: u64) {
        let jump = Gf2::xorshift().pow(blocks);
        for lane in &mut self.lanes {
            *lane = jump.apply(*lane);
        }
    }

    /// The words of the current block.
    #[must_use]
    pub const fn block(&self) -> [u64; BLOCK_WORDS] {
        self.lanes
    }

    /// The `word`-th word of the sequence for `seed`, computed directly: the reference every tier matches.
    #[must_use]
    pub fn word(seed: u64, word: u64) -> u64 {
        Self::at(seed, word / BLOCK_WORDS as u64).lanes[(word % BLOCK_WORDS as u64) as usize]
    }
}

/// One xorshift64 step, the scalar form of what every tier computes per lane.
#[inline]
pub const fn xorshift(mut x: u64) -> u64 {
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

const fn splitmix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// A linear map on 64-bit words over GF(2), as the images of the 64 basis vectors.
#[derive(Clone, Copy)]
struct Gf2([u64; 64]);

impl Gf2 {
    fn xorshift() -> Self {
        Self(core::array::from_fn(|bit| xorshift(1 << bit)))
    }

    fn identity() -> Self {
        Self(core::array::from_fn(|bit| 1 << bit))
    }

    fn apply(&self, x: u64) -> u64 {
        (0..64).filter(|&bit| x >> bit & 1 != 0).fold(0, |acc, bit| acc ^ self.0[bit])
    }

    /// `self` after `first`.
    fn after(&self, first: &Self) -> Self {
        Self(first.0.map(|column| self.apply(column)))
    }

    fn pow(self, mut n: u64) -> Self {
        let (mut result, mut base) = (Self::identity(), self);
        while n != 0 {
            if n & 1 != 0 {
                result = base.after(&result);
            }
            base = base.after(&base);
            n >>= 1;
        }
        result
    }
}
