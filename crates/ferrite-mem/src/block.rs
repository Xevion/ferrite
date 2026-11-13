//! One 64-byte block in registers, per tier, and the only loads and stores of memory under test.
//!
//! Vector loads and temporal stores are inline asm, one aligned `movdqa` per register: the compiler
//! can neither elide nor forward them, and cannot split them either, which it did to a
//! `read_volatile` of `__m512i` whose value was also spilled for the mismatch report (eight scalar
//! loads). Scalar words are `read_volatile` / `write_volatile`. Non-temporal stores are the stream
//! intrinsics. Every method is `#[inline(always)]` so it lands in its tier's `#[target_feature]`
//! wrapper, where the asm helpers inline too.
#![expect(
    clippy::disallowed_methods,
    reason = "the memory-test kernels must touch every word with real loads and stores"
)]
#![expect(
    clippy::cast_ptr_alignment,
    reason = "a Region is 64-byte aligned and whole blocks, so every block pointer is too"
)]
#![expect(clippy::inline_always, reason = "each method must inline into its tier's target_feature wrapper")]
#![expect(clippy::cast_possible_wrap, reason = "the intrinsics take words as i64 bit patterns")]

use core::arch::asm;
use core::arch::x86_64::{
    __m128i, __m256i, __m512i, _mm_add_epi64, _mm_and_si128, _mm_cmpeq_epi8, _mm_movemask_epi8, _mm_set_epi64x,
    _mm_set1_epi64x, _mm_slli_epi64, _mm_srli_epi64, _mm_stream_si128, _mm_xor_si128, _mm256_add_epi64,
    _mm256_and_si256, _mm256_cmpeq_epi64, _mm256_movemask_epi8, _mm256_set_epi64x, _mm256_set1_epi64x,
    _mm256_slli_epi64, _mm256_srli_epi64, _mm256_stream_si256, _mm256_xor_si256, _mm512_add_epi64,
    _mm512_cmpneq_epi64_mask, _mm512_set_epi64, _mm512_set1_epi64, _mm512_slli_epi64, _mm512_srli_epi64,
    _mm512_stream_si512, _mm512_xor_si512,
};
use core::mem::transmute;
use core::ptr;

use crate::prsg;
use crate::region::BLOCK_WORDS;

pub type Words = [u64; BLOCK_WORDS];

/// A block held in registers. All methods need the tier's CPU features, which the caller's
/// `#[target_feature]` wrapper provides; pointers must be 64-byte aligned and inside a region.
pub trait Block: Copy {
    unsafe fn splat(x: u64) -> Self;
    unsafe fn from_words(w: Words) -> Self;
    unsafe fn to_words(self) -> Words;
    unsafe fn load(p: *const u64) -> Self;
    unsafe fn store(p: *mut u64, v: Self);
    /// A non-temporal store; tiers without one store normally.
    unsafe fn stream(p: *mut u64, v: Self);
    unsafe fn same(a: Self, b: Self) -> bool;
    unsafe fn xor(a: Self, b: Self) -> Self;
    unsafe fn add(a: Self, b: Self) -> Self;
    /// One xorshift64 step per lane.
    unsafe fn xorshift(a: Self) -> Self;
}

#[derive(Clone, Copy)]
pub struct Scalar(Words);

impl Block for Scalar {
    #[inline(always)]
    unsafe fn splat(x: u64) -> Self {
        Self([x; BLOCK_WORDS])
    }

    #[inline(always)]
    unsafe fn from_words(w: Words) -> Self {
        Self(w)
    }

    #[inline(always)]
    unsafe fn to_words(self) -> Words {
        self.0
    }

    #[inline(always)]
    unsafe fn load(p: *const u64) -> Self {
        // SAFETY: the caller's pointer covers a whole block.
        Self(core::array::from_fn(|i| unsafe { ptr::read_volatile(p.add(i)) }))
    }

    #[inline(always)]
    unsafe fn store(p: *mut u64, v: Self) {
        for (i, w) in v.0.into_iter().enumerate() {
            // SAFETY: as `load`.
            unsafe { ptr::write_volatile(p.add(i), w) };
        }
    }

    #[inline(always)]
    unsafe fn stream(p: *mut u64, v: Self) {
        // SAFETY: as `store`.
        unsafe { Self::store(p, v) };
    }

    #[inline(always)]
    unsafe fn same(a: Self, b: Self) -> bool {
        // An array `==` becomes a `bcmp` call.
        a.0.iter().zip(b.0).fold(0, |diff, (x, y)| diff | (x ^ y)) == 0
    }

    #[inline(always)]
    unsafe fn xor(a: Self, b: Self) -> Self {
        Self(core::array::from_fn(|i| a.0[i] ^ b.0[i]))
    }

    #[inline(always)]
    unsafe fn add(a: Self, b: Self) -> Self {
        Self(core::array::from_fn(|i| a.0[i].wrapping_add(b.0[i])))
    }

    #[inline(always)]
    unsafe fn xorshift(a: Self) -> Self {
        Self(a.0.map(prsg::xorshift))
    }
}

#[derive(Clone, Copy)]
pub struct Sse2([__m128i; 4]);

impl Block for Sse2 {
    #[inline(always)]
    unsafe fn splat(x: u64) -> Self {
        // SAFETY: SSE2 is the x86-64 baseline.
        Self([unsafe { _mm_set1_epi64x(x as i64) }; 4])
    }

    #[inline(always)]
    unsafe fn from_words(w: Words) -> Self {
        // SAFETY: as `splat`.
        Self(core::array::from_fn(|i| unsafe { _mm_set_epi64x(w[2 * i + 1] as i64, w[2 * i] as i64) }))
    }

    #[inline(always)]
    unsafe fn to_words(self) -> Words {
        // SAFETY: four 128-bit vectors are eight words, lane order kept.
        unsafe { transmute::<[__m128i; 4], Words>(self.0) }
    }

    #[inline(always)]
    unsafe fn load(p: *const u64) -> Self {
        // SAFETY: the block is 64-byte aligned, so each quarter is 16-byte aligned.
        Self(core::array::from_fn(|i| unsafe { load128(p.add(2 * i)) }))
    }

    #[inline(always)]
    unsafe fn store(p: *mut u64, v: Self) {
        for (i, x) in v.0.into_iter().enumerate() {
            // SAFETY: as `load`.
            unsafe { store128(p.add(2 * i), x) };
        }
    }

    #[inline(always)]
    unsafe fn stream(p: *mut u64, v: Self) {
        let p = p.cast::<__m128i>();
        for (i, x) in v.0.into_iter().enumerate() {
            // SAFETY: as `load`.
            unsafe { _mm_stream_si128(p.add(i), x) };
        }
    }

    #[inline(always)]
    unsafe fn same(a: Self, b: Self) -> bool {
        // SSE2 has no 64-bit compare; a byte compare finds the same mismatches.
        // SAFETY: as `splat`.
        unsafe {
            let eq = |i: usize| _mm_cmpeq_epi8(a.0[i], b.0[i]);
            let all = _mm_and_si128(_mm_and_si128(eq(0), eq(1)), _mm_and_si128(eq(2), eq(3)));
            _mm_movemask_epi8(all) == 0xffff
        }
    }

    #[inline(always)]
    unsafe fn xor(a: Self, b: Self) -> Self {
        // SAFETY: as `splat`.
        Self(core::array::from_fn(|i| unsafe { _mm_xor_si128(a.0[i], b.0[i]) }))
    }

    #[inline(always)]
    unsafe fn add(a: Self, b: Self) -> Self {
        // SAFETY: as `splat`.
        Self(core::array::from_fn(|i| unsafe { _mm_add_epi64(a.0[i], b.0[i]) }))
    }

    #[inline(always)]
    unsafe fn xorshift(a: Self) -> Self {
        // SAFETY: as `splat`.
        Self(a.0.map(|mut x| unsafe {
            x = _mm_xor_si128(x, _mm_slli_epi64::<13>(x));
            x = _mm_xor_si128(x, _mm_srli_epi64::<7>(x));
            _mm_xor_si128(x, _mm_slli_epi64::<17>(x))
        }))
    }
}

#[derive(Clone, Copy)]
pub struct Avx2([__m256i; 2]);

impl Block for Avx2 {
    #[inline(always)]
    unsafe fn splat(x: u64) -> Self {
        // SAFETY: the caller runs with AVX2 enabled.
        Self([unsafe { _mm256_set1_epi64x(x as i64) }; 2])
    }

    #[inline(always)]
    unsafe fn from_words(w: Words) -> Self {
        // SAFETY: as `splat`.
        unsafe {
            Self([
                _mm256_set_epi64x(w[3] as i64, w[2] as i64, w[1] as i64, w[0] as i64),
                _mm256_set_epi64x(w[7] as i64, w[6] as i64, w[5] as i64, w[4] as i64),
            ])
        }
    }

    #[inline(always)]
    unsafe fn to_words(self) -> Words {
        // SAFETY: two 256-bit vectors are eight words, lane order kept.
        unsafe { transmute::<[__m256i; 2], Words>(self.0) }
    }

    #[inline(always)]
    unsafe fn load(p: *const u64) -> Self {
        // SAFETY: the block is 64-byte aligned, so each half is 32-byte aligned; AVX is on.
        unsafe { Self([load256(p), load256(p.add(4))]) }
    }

    #[inline(always)]
    unsafe fn store(p: *mut u64, v: Self) {
        // SAFETY: as `load`.
        unsafe {
            store256(p, v.0[0]);
            store256(p.add(4), v.0[1]);
        }
    }

    #[inline(always)]
    unsafe fn stream(p: *mut u64, v: Self) {
        let p = p.cast::<__m256i>();
        // SAFETY: as `load`.
        unsafe {
            _mm256_stream_si256(p, v.0[0]);
            _mm256_stream_si256(p.add(1), v.0[1]);
        }
    }

    #[inline(always)]
    unsafe fn same(a: Self, b: Self) -> bool {
        // SAFETY: as `splat`.
        unsafe {
            let all = _mm256_and_si256(_mm256_cmpeq_epi64(a.0[0], b.0[0]), _mm256_cmpeq_epi64(a.0[1], b.0[1]));
            _mm256_movemask_epi8(all) == -1
        }
    }

    #[inline(always)]
    unsafe fn xor(a: Self, b: Self) -> Self {
        // SAFETY: as `splat`.
        unsafe { Self([_mm256_xor_si256(a.0[0], b.0[0]), _mm256_xor_si256(a.0[1], b.0[1])]) }
    }

    #[inline(always)]
    unsafe fn add(a: Self, b: Self) -> Self {
        // SAFETY: as `splat`.
        unsafe { Self([_mm256_add_epi64(a.0[0], b.0[0]), _mm256_add_epi64(a.0[1], b.0[1])]) }
    }

    #[inline(always)]
    unsafe fn xorshift(a: Self) -> Self {
        // SAFETY: as `splat`.
        unsafe { Self([xorshift256(a.0[0]), xorshift256(a.0[1])]) }
    }
}

#[derive(Clone, Copy)]
pub struct Avx512(__m512i);

impl Block for Avx512 {
    #[inline(always)]
    unsafe fn splat(x: u64) -> Self {
        // SAFETY: the caller runs with AVX-512F enabled.
        Self(unsafe { _mm512_set1_epi64(x as i64) })
    }

    #[inline(always)]
    unsafe fn from_words(w: Words) -> Self {
        let w = w.map(|x| x as i64);
        // SAFETY: as `splat`.
        Self(unsafe { _mm512_set_epi64(w[7], w[6], w[5], w[4], w[3], w[2], w[1], w[0]) })
    }

    #[inline(always)]
    unsafe fn to_words(self) -> Words {
        // SAFETY: a 512-bit vector is eight words, lane order kept.
        unsafe { transmute::<__m512i, Words>(self.0) }
    }

    #[inline(always)]
    unsafe fn load(p: *const u64) -> Self {
        // SAFETY: the block is 64-byte aligned; AVX-512F is on.
        Self(unsafe { load512(p) })
    }

    #[inline(always)]
    unsafe fn store(p: *mut u64, v: Self) {
        // SAFETY: as `load`.
        unsafe { store512(p, v.0) };
    }

    #[inline(always)]
    unsafe fn stream(p: *mut u64, v: Self) {
        // SAFETY: as `load`.
        unsafe { _mm512_stream_si512(p.cast::<__m512i>(), v.0) };
    }

    #[inline(always)]
    unsafe fn same(a: Self, b: Self) -> bool {
        // SAFETY: as `splat`.
        unsafe { _mm512_cmpneq_epi64_mask(a.0, b.0) == 0 }
    }

    #[inline(always)]
    unsafe fn xor(a: Self, b: Self) -> Self {
        // SAFETY: as `splat`.
        Self(unsafe { _mm512_xor_si512(a.0, b.0) })
    }

    #[inline(always)]
    unsafe fn add(a: Self, b: Self) -> Self {
        // SAFETY: as `splat`.
        Self(unsafe { _mm512_add_epi64(a.0, b.0) })
    }

    #[inline(always)]
    unsafe fn xorshift(a: Self) -> Self {
        // SAFETY: as `splat`.
        unsafe {
            let mut x = a.0;
            x = _mm512_xor_si512(x, _mm512_slli_epi64::<13>(x));
            x = _mm512_xor_si512(x, _mm512_srli_epi64::<7>(x));
            Self(_mm512_xor_si512(x, _mm512_slli_epi64::<17>(x)))
        }
    }
}

/// # Safety
/// `p` is 16-byte aligned and readable.
#[inline]
unsafe fn load128(p: *const u64) -> __m128i {
    let v: __m128i;
    // SAFETY: the caller's contract; SSE2 is the x86-64 baseline.
    unsafe {
        asm!("movdqa {v}, xmmword ptr [{p}]", v = out(xmm_reg) v, p = in(reg) p, options(nostack, preserves_flags, readonly));
    };
    v
}

/// # Safety
/// `p` is 16-byte aligned and writable.
#[inline]
unsafe fn store128(p: *mut u64, v: __m128i) {
    // SAFETY: as `load128`.
    unsafe {
        asm!("movdqa xmmword ptr [{p}], {v}", v = in(xmm_reg) v, p = in(reg) p, options(nostack, preserves_flags));
    };
}

/// # Safety
/// `p` is 32-byte aligned and readable, and AVX state is enabled.
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn load256(p: *const u64) -> __m256i {
    let v: __m256i;
    // SAFETY: the caller's contract.
    unsafe {
        asm!("vmovdqa {v}, ymmword ptr [{p}]", v = out(ymm_reg) v, p = in(reg) p, options(nostack, preserves_flags, readonly));
    };
    v
}

/// # Safety
/// `p` is 32-byte aligned and writable, and AVX state is enabled.
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn store256(p: *mut u64, v: __m256i) {
    // SAFETY: the caller's contract.
    unsafe {
        asm!("vmovdqa ymmword ptr [{p}], {v}", v = in(ymm_reg) v, p = in(reg) p, options(nostack, preserves_flags));
    };
}

/// # Safety
/// `p` is 64-byte aligned and readable, and AVX-512 state is enabled.
#[target_feature(enable = "avx512f")]
#[inline]
unsafe fn load512(p: *const u64) -> __m512i {
    let v: __m512i;
    // SAFETY: the caller's contract.
    unsafe {
        asm!("vmovdqa64 {v}, zmmword ptr [{p}]", v = out(zmm_reg) v, p = in(reg) p, options(nostack, preserves_flags, readonly));
    };
    v
}

/// # Safety
/// `p` is 64-byte aligned and writable, and AVX-512 state is enabled.
#[target_feature(enable = "avx512f")]
#[inline]
unsafe fn store512(p: *mut u64, v: __m512i) {
    // SAFETY: the caller's contract.
    unsafe {
        asm!("vmovdqa64 zmmword ptr [{p}], {v}", v = in(zmm_reg) v, p = in(reg) p, options(nostack, preserves_flags));
    };
}

#[target_feature(enable = "avx2")]
#[inline]
fn xorshift256(mut x: __m256i) -> __m256i {
    x = _mm256_xor_si256(x, _mm256_slli_epi64::<13>(x));
    x = _mm256_xor_si256(x, _mm256_srli_epi64::<7>(x));
    _mm256_xor_si256(x, _mm256_slli_epi64::<17>(x))
}
