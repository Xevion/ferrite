//! The building blocks written once over [`Block`], and their per-tier `#[target_feature]` wrappers.
//!
//! Every kernel takes a 64-byte-aligned base and a block count from a [`Region`](crate::Region).
#![expect(clippy::inline_always, reason = "the generic kernels must inline into their tier's target_feature wrapper")]

use core::arch::x86_64::_mm_sfence;

use crate::block::{Block, Words};
use crate::fault::{CpuAddr, Fault, Faults};
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
        }
    };
}

tier!(scalar, crate::block::Scalar, "sse2");
tier!(sse2, crate::block::Sse2, "sse2");
tier!(avx2, crate::block::Avx2, "avx2");
tier!(avx512, crate::block::Avx512, "avx512f");
