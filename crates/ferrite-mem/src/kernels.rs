//! The building blocks written once over [`Block`], and their per-tier `#[target_feature]` wrappers.
//!
//! Every kernel takes a 64-byte-aligned base and a block count from a [`Region`](crate::Region).
#![expect(clippy::inline_always, reason = "the generic kernels must inline into their tier's target_feature wrapper")]

use core::arch::x86_64::_mm_sfence;

use crate::block::{Block, Words};
use crate::fault::{CpuAddr, Fault, Faults};
use crate::ops::{Inversion, Order};
use crate::region::{BLOCK_BYTES, BLOCK_WORDS};

#[inline(always)]
unsafe fn put<B: Block, const NT: bool>(p: *mut u64, v: B) {
    // SAFETY: the caller's pointer is an aligned block inside its region.
    unsafe {
        if NT {
            B::stream(p, v);
        } else {
            B::store(p, v);
        }
    }
}

#[inline(always)]
fn fence<const NT: bool>() {
    if NT {
        // SAFETY: SSE is the x86-64 baseline.
        unsafe { _mm_sfence() };
    }
}

/// Load the block at `p` and report every word that differs from `expected`.
#[inline(always)]
unsafe fn check<B: Block>(p: *mut u64, expected: B, faults: &mut Faults<'_>) -> B {
    // SAFETY: as `put`.
    unsafe {
        let actual = B::load(p);
        if !B::same(actual, expected) {
            locate(p, &expected.to_words(), &actual.to_words(), faults);
        }
        actual
    }
}

/// The scalar fallback that finds which words of a mismatched block differ.
#[cold]
#[inline(never)]
fn locate(p: *mut u64, expected: &Words, actual: &Words, faults: &mut Faults<'_>) {
    for (i, (&expected, &actual)) in expected.iter().zip(actual).enumerate() {
        if expected != actual {
            faults.record(Fault { addr: CpuAddr::of(p.wrapping_add(i)), expected, actual });
        }
    }
}

/// The addresses of the words of the block at `p`, and the step to the next block.
#[inline(always)]
unsafe fn addresses<B: Block>(p: *mut u64) -> (B, B) {
    let base = p.addr() as u64;
    // SAFETY: the caller has the tier's features.
    unsafe { (B::from_words(core::array::from_fn(|i| base + 8 * i as u64)), B::splat(BLOCK_BYTES as u64)) }
}

#[inline(always)]
pub unsafe fn fill_const<B: Block, const NT: bool>(base: *mut u64, blocks: usize, value: u64) {
    // SAFETY: `base` and `blocks` describe a region.
    unsafe {
        let v = B::splat(value);
        for b in 0..blocks {
            put::<B, NT>(base.add(b * BLOCK_WORDS), v);
        }
    }
    fence::<NT>();
}

#[inline(always)]
pub unsafe fn verify_const<B: Block>(base: *mut u64, blocks: usize, value: u64, faults: &mut Faults<'_>) {
    // SAFETY: as `fill_const`.
    unsafe {
        let v = B::splat(value);
        for b in 0..blocks {
            check(base.add(b * BLOCK_WORDS), v, faults);
        }
    }
}

#[inline(always)]
pub unsafe fn fill_address<B: Block, const NT: bool>(base: *mut u64, blocks: usize, key: u64) {
    // SAFETY: as `fill_const`.
    unsafe {
        let (mut a, step) = addresses::<B>(base);
        let k = B::splat(key);
        for b in 0..blocks {
            put::<B, NT>(base.add(b * BLOCK_WORDS), B::xor(a, k));
            a = B::add(a, step);
        }
    }
    fence::<NT>();
}

#[inline(always)]
pub unsafe fn verify_address<B: Block>(base: *mut u64, blocks: usize, key: u64, faults: &mut Faults<'_>) {
    // SAFETY: as `fill_const`.
    unsafe {
        let (mut a, step) = addresses::<B>(base);
        let k = B::splat(key);
        for b in 0..blocks {
            check(base.add(b * BLOCK_WORDS), B::xor(a, k), faults);
            a = B::add(a, step);
        }
    }
}

#[inline(always)]
pub unsafe fn fill_prsg<B: Block, const NT: bool>(base: *mut u64, blocks: usize, lanes: &mut Words, key: u64) {
    // SAFETY: as `fill_const`.
    unsafe {
        let mut s = B::from_words(*lanes);
        let k = B::splat(key);
        for b in 0..blocks {
            put::<B, NT>(base.add(b * BLOCK_WORDS), B::xor(s, k));
            s = B::xorshift(s);
        }
        *lanes = s.to_words();
    }
    fence::<NT>();
}

#[inline(always)]
pub unsafe fn verify_prsg<B: Block>(
    base: *mut u64,
    blocks: usize,
    lanes: &mut Words,
    key: u64,
    faults: &mut Faults<'_>,
) {
    // SAFETY: as `fill_const`.
    unsafe {
        let mut s = B::from_words(*lanes);
        let k = B::splat(key);
        for b in 0..blocks {
            check(base.add(b * BLOCK_WORDS), B::xor(s, k), faults);
            s = B::xorshift(s);
        }
        *lanes = s.to_words();
    }
}

/// Read and check each block against `expect`, then write `write` over it, in block order.
#[inline(always)]
pub unsafe fn invert<B: Block, const NT: bool>(
    base: *mut u64,
    blocks: usize,
    expect: u64,
    write: u64,
    descending: bool,
    faults: &mut Faults<'_>,
) {
    // SAFETY: as `fill_const`.
    unsafe {
        let (e, w) = (B::splat(expect), B::splat(write));
        for i in 0..blocks {
            let p = base.add(if descending { blocks - 1 - i } else { i } * BLOCK_WORDS);
            check(p, e, faults);
            put::<B, NT>(p, w);
        }
    }
    fence::<NT>();
}

#[inline(always)]
pub unsafe fn copy<B: Block, const NT: bool>(src: *mut u64, dst: *mut u64, blocks: usize) {
    // SAFETY: `src` and `dst` are disjoint regions of `blocks` blocks each.
    unsafe {
        for b in 0..blocks {
            put::<B, NT>(dst.add(b * BLOCK_WORDS), B::load(src.add(b * BLOCK_WORDS)));
        }
    }
    fence::<NT>();
}

/// Which per-lane sequence a sequence kernel steps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeqKind {
    /// xorshift64, forwards and inverted.
    Prsg,
    /// A rotation by one block's worth of words (8 bits).
    Rotate,
}

/// One block's step of a sequence, both ways.
trait Walk<B: Block> {
    unsafe fn next(s: B) -> B;
    unsafe fn prev(s: B) -> B;
}

struct XorShift;

impl<B: Block> Walk<B> for XorShift {
    #[inline(always)]
    unsafe fn next(s: B) -> B {
        // SAFETY: the caller has the tier's features.
        unsafe { B::xorshift(s) }
    }

    #[inline(always)]
    unsafe fn prev(s: B) -> B {
        // SAFETY: as `next`.
        unsafe { B::unxorshift(s) }
    }
}

struct Rotate;

impl<B: Block> Walk<B> for Rotate {
    #[inline(always)]
    unsafe fn next(s: B) -> B {
        // SAFETY: the caller has the tier's features.
        unsafe { B::rotl(s, BLOCK_WORDS as u32) }
    }

    #[inline(always)]
    unsafe fn prev(s: B) -> B {
        // SAFETY: as `next`.
        unsafe { B::rotr(s, BLOCK_WORDS as u32) }
    }
}

#[inline(always)]
unsafe fn fill_seq<B: Block, G: Walk<B>, const NT: bool>(base: *mut u64, blocks: usize, lanes: &mut Words, key: u64) {
    // SAFETY: as `fill_const`.
    unsafe {
        let mut s = B::from_words(*lanes);
        let k = B::splat(key);
        for b in 0..blocks {
            put::<B, NT>(base.add(b * BLOCK_WORDS), B::xor(s, k));
            s = G::next(s);
        }
        *lanes = s.to_words();
    }
    fence::<NT>();
}

/// Check each block against the sequence XOR `step.expect` and rewrite it XOR `step.write`.
/// Ascending starts with `lanes` at the first block and leaves them past the last; descending
/// starts past the last block, steps back before each one, and leaves them at the first.
#[inline(always)]
unsafe fn invert_seq<B: Block, G: Walk<B>, const NT: bool>(
    base: *mut u64,
    blocks: usize,
    lanes: &mut Words,
    step: Inversion,
    faults: &mut Faults<'_>,
) {
    // SAFETY: as `fill_const`.
    unsafe {
        let (e, w) = (B::splat(step.expect), B::splat(step.write));
        let mut s = B::from_words(*lanes);
        match step.order {
            Order::Ascending => {
                for i in 0..blocks {
                    let p = base.add(i * BLOCK_WORDS);
                    check(p, B::xor(s, e), faults);
                    put::<B, NT>(p, B::xor(s, w));
                    s = G::next(s);
                }
            },
            Order::Descending => {
                for i in (0..blocks).rev() {
                    s = G::prev(s);
                    let p = base.add(i * BLOCK_WORDS);
                    check(p, B::xor(s, e), faults);
                    put::<B, NT>(p, B::xor(s, w));
                }
            },
        }
        *lanes = s.to_words();
    }
    fence::<NT>();
}

/// Every `n`-th word, the one with index `offset` modulo `n`, of a chunk whose first word has
/// index `first` modulo `n`. `n` is at least a block, so a block holds at most one of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stride {
    pub n: usize,
    pub offset: usize,
    pub first: usize,
}

impl Stride {
    /// Index of the first selected word in the chunk.
    const fn start(self) -> usize {
        (self.offset + self.n - self.first % self.n) % self.n
    }
}

/// Store `value` in every word except those `stride` selects, which are left untouched.
#[inline(always)]
unsafe fn fill_except<B: Block, const NT: bool>(base: *mut u64, blocks: usize, stride: Stride, value: u64) {
    // SAFETY: as `fill_const`.
    unsafe {
        let v = B::splat(value);
        let mut skip = stride.start();
        for b in 0..blocks {
            let p = base.add(b * BLOCK_WORDS);
            if skip >= BLOCK_WORDS {
                put::<B, NT>(p, v);
                skip -= BLOCK_WORDS;
            } else {
                crate::block::prefetch_write(base.wrapping_add(b * BLOCK_WORDS + skip + SPARSE_AHEAD * stride.n));
                B::store_except(p, v, skip);
                skip += stride.n - BLOCK_WORDS;
            }
        }
    }
    fence::<NT>();
}

/// Strides ahead of the walk whose lines the sparse passes ask for, so the loads and stores
/// overlap their misses instead of taking them one at a time.
const SPARSE_AHEAD: usize = 24;

/// Store `value` in the words `stride` selects, and only those.
///
/// # Safety
/// `base` and `words` describe a region.
pub unsafe fn fill_stride(base: *mut u64, words: usize, stride: Stride, value: u64) {
    let mut w = stride.start();
    while w < words {
        crate::block::prefetch_write(base.wrapping_add(w + SPARSE_AHEAD * stride.n));
        // SAFETY: `w < words`.
        unsafe { crate::block::write_word(base.add(w), value) };
        w += stride.n;
    }
}

/// Check that the words `stride` selects hold `value`.
///
/// # Safety
/// As [`fill_stride`].
pub unsafe fn verify_stride(base: *mut u64, words: usize, stride: Stride, value: u64, faults: &mut Faults<'_>) {
    let mut w = stride.start();
    while w < words {
        crate::block::prefetch_read(base.wrapping_add(w + SPARSE_AHEAD * stride.n));
        // SAFETY: `w < words`.
        let (p, actual) = unsafe { (base.add(w), crate::block::read_word(base.add(w))) };
        if actual != value {
            faults.record(Fault { addr: CpuAddr::of(p), expected: value, actual });
        }
        w += stride.n;
    }
}

/// Check that the words `check` selects hold `expect`, and store `value` in those `fill` selects,
/// both in address order, a word's check before its store.
///
/// The same reads, writes and fault reports as [`verify_stride`] then [`fill_stride`], in one
/// walk: when both pick the same word, or neighbours in a line, the line is fetched once.
///
/// # Safety
/// As [`fill_stride`].
pub unsafe fn verify_fill_stride(
    base: *mut u64,
    words: usize,
    check: Stride,
    expect: u64,
    fill: Stride,
    value: u64,
    faults: &mut Faults<'_>,
) {
    let (mut c, mut f) = (check.start(), fill.start());
    while c < words || f < words {
        crate::block::prefetch_write(base.wrapping_add(c.min(f) + SPARSE_AHEAD * check.n));
        if c <= f {
            // SAFETY: `c <= f` and one of them is below `words`, so `c` is.
            let (p, actual) = unsafe { (base.add(c), crate::block::read_word(base.add(c))) };
            if actual != expect {
                faults.record(Fault { addr: CpuAddr::of(p), expected: expect, actual });
            }
            if c == f {
                // SAFETY: the word just read.
                unsafe { crate::block::write_word(base.add(f), value) };
                f += fill.n;
            }
            c += check.n;
        } else {
            // SAFETY: `f < c`, and `f < words` as the loop runs.
            unsafe { crate::block::write_word(base.add(f), value) };
            f += fill.n;
        }
    }
}

/// The block-move pattern: each 16-word group is four `a`, two `!a`, four `a`, two `!a`, two
/// `a`, two `!a`, with `a` a single one bit rotated left once per group. `first_group` is the
/// index of the group at `base`; the region's last group may be a single block.
///
/// # Safety
/// As [`fill_stride`], with `blocks` the region's blocks.
pub unsafe fn fill_move_pattern(base: *mut u64, blocks: usize, first_group: usize) {
    for b in 0..blocks {
        let a = 1_u64.rotate_left(((first_group + b / 2) % 64) as u32);
        let (x, y) = (a, !a);
        let words = if b % 2 == 0 { [x, x, x, x, y, y, x, x] } else { [x, x, y, y, x, x, y, y] };
        // SAFETY: block `b` is inside the region.
        unsafe {
            put::<crate::block::Scalar, false>(base.add(b * BLOCK_WORDS), crate::block::Scalar::from_words(words));
        };
    }
}

/// Check that the two words of each aligned pair are equal, reporting the second against the first.
///
/// # Safety
/// As [`fill_move_pattern`].
pub unsafe fn verify_pairs(base: *mut u64, blocks: usize, faults: &mut Faults<'_>) {
    for b in 0..blocks {
        // SAFETY: block `b` is inside the region.
        let (p, words) = unsafe {
            let p = base.add(b * BLOCK_WORDS);
            (p, crate::block::Scalar::load(p).to_words())
        };
        for (i, pair) in words.as_chunks::<2>().0.iter().enumerate() {
            if pair[0] != pair[1] {
                let addr = CpuAddr::of(p.wrapping_add(2 * i + 1));
                faults.record(Fault { addr, expected: pair[0], actual: pair[1] });
            }
        }
    }
}

/// Word accesses for the address walk, which must reach DRAM rather than the caches.
pub trait Cells {
    /// # Safety
    /// `p` is an aligned word inside a region.
    unsafe fn write(&mut self, p: *mut u64, v: u64);
    /// # Safety
    /// As `write`.
    unsafe fn read(&mut self, p: *mut u64) -> u64;
}

/// Each access is followed by a flush of its line, so the next read of a word misses every cache.
pub struct Dram;

impl Cells for Dram {
    unsafe fn write(&mut self, p: *mut u64, v: u64) {
        // SAFETY: the caller's contract.
        unsafe {
            crate::block::write_word(p, v);
            crate::block::flush_word(p);
        }
    }

    unsafe fn read(&mut self, p: *mut u64) -> u64 {
        // SAFETY: the caller's contract.
        unsafe {
            let v = crate::block::read_word(p);
            crate::block::flush_word(p);
            v
        }
    }
}

/// Walking ones on the address: for each pair of addresses `anchor` and `anchor + 2^k` (and the
/// anchor itself), write the first with its address XOR `invert`, the second with the complement
/// of its own, and check that the first kept its value. `anchor` is `span_bytes`-aligned.
///
/// # Safety
/// `anchor..anchor + span_bytes` is inside a region.
pub unsafe fn address_walk<C: Cells>(
    cells: &mut C,
    anchor: *mut u64,
    span_bytes: usize,
    invert: u64,
    faults: &mut Faults<'_>,
) {
    let offsets = || core::iter::once(0).chain((3..usize::BITS).map(|k| 1 << k).take_while(move |&o| o < span_bytes));
    for o1 in offsets() {
        let p1 = anchor.wrapping_byte_add(o1);
        let expect = invert ^ p1.addr() as u64;
        // SAFETY: every offset is inside the span.
        unsafe { cells.write(p1, expect) };
        for o2 in offsets().filter(|&o2| o2 != o1) {
            let p2 = anchor.wrapping_byte_add(o2);
            // SAFETY: as above.
            let actual = unsafe {
                cells.write(p2, !invert ^ p2.addr() as u64);
                cells.read(p1)
            };
            if actual != expect {
                faults.record(Fault { addr: CpuAddr::of(p1), expected: expect, actual });
                // SAFETY: as above.
                unsafe { cells.write(p1, expect) };
            }
        }
    }
}

/// `rep stosq` over the region.
pub unsafe fn rep_fill(base: *mut u64, blocks: usize, value: u64) {
    // SAFETY: the region is `blocks * 8` writable words; DF is clear per the ABI.
    unsafe {
        core::arch::asm!("rep stosq", inout("rcx") blocks * BLOCK_WORDS => _, inout("rdi") base => _,
            in("rax") value, options(nostack, preserves_flags));
    }
}

/// `rep movsq` between two disjoint regions.
pub unsafe fn rep_copy(src: *mut u64, dst: *mut u64, blocks: usize) {
    // SAFETY: as `rep_fill`, with `src` readable for the same length.
    unsafe {
        core::arch::asm!("rep movsq", inout("rcx") blocks * BLOCK_WORDS => _, inout("rsi") src => _,
            inout("rdi") dst => _, options(nostack, preserves_flags));
    }
}

/// Monomorphic wrappers for one tier, compiled with its target features.
macro_rules! tier {
    ($name:ident, $block:ty, $feature:literal) => {
        pub mod $name {
            use super::*;

            #[target_feature(enable = $feature)]
            pub unsafe fn fill_const<const NT: bool>(base: *mut u64, blocks: usize, value: u64) {
                // SAFETY: forwarded; the caller holds a kernel for this tier.
                unsafe { super::fill_const::<$block, NT>(base, blocks, value) }
            }

            #[target_feature(enable = $feature)]
            pub unsafe fn verify_const(base: *mut u64, blocks: usize, value: u64, faults: &mut Faults<'_>) {
                // SAFETY: as `fill_const`.
                unsafe { super::verify_const::<$block>(base, blocks, value, faults) }
            }

            #[target_feature(enable = $feature)]
            pub unsafe fn fill_address<const NT: bool>(base: *mut u64, blocks: usize, key: u64) {
                // SAFETY: as `fill_const`.
                unsafe { super::fill_address::<$block, NT>(base, blocks, key) }
            }

            #[target_feature(enable = $feature)]
            pub unsafe fn verify_address(base: *mut u64, blocks: usize, key: u64, faults: &mut Faults<'_>) {
                // SAFETY: as `fill_const`.
                unsafe { super::verify_address::<$block>(base, blocks, key, faults) }
            }

            #[target_feature(enable = $feature)]
            pub unsafe fn fill_prsg<const NT: bool>(base: *mut u64, blocks: usize, lanes: &mut Words, key: u64) {
                // SAFETY: as `fill_const`.
                unsafe { super::fill_prsg::<$block, NT>(base, blocks, lanes, key) }
            }

            #[target_feature(enable = $feature)]
            pub unsafe fn verify_prsg(
                base: *mut u64,
                blocks: usize,
                lanes: &mut Words,
                key: u64,
                faults: &mut Faults<'_>,
            ) {
                // SAFETY: as `fill_const`.
                unsafe { super::verify_prsg::<$block>(base, blocks, lanes, key, faults) }
            }

            #[target_feature(enable = $feature)]
            pub unsafe fn invert<const NT: bool>(
                base: *mut u64,
                blocks: usize,
                expect: u64,
                write: u64,
                descending: bool,
                faults: &mut Faults<'_>,
            ) {
                // SAFETY: as `fill_const`.
                unsafe { super::invert::<$block, NT>(base, blocks, expect, write, descending, faults) }
            }

            #[target_feature(enable = $feature)]
            pub unsafe fn copy<const NT: bool>(src: *mut u64, dst: *mut u64, blocks: usize) {
                // SAFETY: as `fill_const`.
                unsafe { super::copy::<$block, NT>(src, dst, blocks) }
            }
            #[target_feature(enable = $feature)]
            pub unsafe fn fill_seq<const NT: bool>(
                base: *mut u64,
                blocks: usize,
                kind: SeqKind,
                lanes: &mut Words,
                key: u64,
            ) {
                // SAFETY: as `fill_const`.
                unsafe {
                    match kind {
                        SeqKind::Prsg => super::fill_seq::<$block, XorShift, NT>(base, blocks, lanes, key),
                        SeqKind::Rotate => super::fill_seq::<$block, Rotate, NT>(base, blocks, lanes, key),
                    }
                }
            }

            #[target_feature(enable = $feature)]
            pub unsafe fn invert_seq<const NT: bool>(
                base: *mut u64,
                blocks: usize,
                kind: SeqKind,
                lanes: &mut Words,
                step: Inversion,
                faults: &mut Faults<'_>,
            ) {
                // SAFETY: as `fill_const`.
                unsafe {
                    match kind {
                        SeqKind::Prsg => super::invert_seq::<$block, XorShift, NT>(base, blocks, lanes, step, faults),
                        SeqKind::Rotate => super::invert_seq::<$block, Rotate, NT>(base, blocks, lanes, step, faults),
                    }
                }
            }

            #[target_feature(enable = $feature)]
            pub unsafe fn fill_except<const NT: bool>(base: *mut u64, blocks: usize, stride: Stride, value: u64) {
                // SAFETY: as `fill_const`.
                unsafe { super::fill_except::<$block, NT>(base, blocks, stride, value) }
            }
        }
    };
}

tier!(scalar, crate::block::Scalar, "sse2");
tier!(sse2, crate::block::Sse2, "sse2");
tier!(avx2, crate::block::Avx2, "avx2");
tier!(avx512, crate::block::Avx512, "avx512f");
