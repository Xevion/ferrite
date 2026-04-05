#![cfg_attr(coverage_nightly, coverage(off))]
// SIMD intrinsics require casting *mut u64 -> *mut __m512i with stricter alignment.
// Alignment is guaranteed by the mmap allocation (page-aligned = 4096-byte aligned).
#![allow(clippy::cast_ptr_alignment)]
#[cfg(target_arch = "x86_64")]
use std::ptr;

#[cfg(target_arch = "x86_64")]
use rayon::prelude::*;

#[cfg(target_arch = "x86_64")]
use crate::Failure;

/// Number of u64 words processed per Rayon task.
/// Must be a multiple of 8 (one AVX-512 register = 8 * u64 = 64 bytes) so that
/// every chunk boundary is 64-byte aligned and NT store / aligned load intrinsics
/// never straddle a chunk boundary.
#[cfg(target_arch = "x86_64")]
pub(crate) const CHUNK: usize = 64 * 1024; // 64 K u64s = 512 KiB

/// Returns true if AVX-512F is available. With `target-cpu=native` this is a
/// compile-time constant and the dead branches are eliminated by LLVM.
#[cfg(target_arch = "x86_64")]
#[inline]
pub(crate) fn avx512_available() -> bool {
    is_x86_feature_detected!("avx512f")
}

/// Fill `buf` with `pattern` using AVX-512 non-temporal (streaming) stores.
///
/// NT stores write directly to DRAM, bypassing all CPU cache levels.
/// This avoids the read-for-ownership penalty of regular cached writes and
/// keeps caches warm for non-test data. Ends with `_mm_sfence` to flush
/// the write-combining buffers before returning.
///
/// Falls back to scalar volatile writes if the buffer is not 64-byte aligned
/// (e.g., heap-allocated test buffers). Production paths using mmap-backed
/// buffers are always page-aligned (>= 4096 bytes) and always take the NT path.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f")]
pub(crate) unsafe fn fill_nt(buf: &mut [u64], pattern: u64) {
    use std::arch::x86_64::{__m512i, _mm_sfence, _mm512_set1_epi64, _mm512_stream_si512};
    if !(buf.as_ptr() as usize).is_multiple_of(64) {
        // Unaligned fallback: NT stores require 64-byte alignment.
        for word in buf.iter_mut() {
            unsafe { ptr::write_volatile(std::ptr::from_mut::<u64>(word), pattern) };
        }
        return;
    }
    let vp = _mm512_set1_epi64(pattern as i64);
    let base = buf.as_mut_ptr().cast::<__m512i>();
    let n = buf.len() / 8;
    for i in 0..n {
        // SAFETY: base + i is within the buffer and 64-byte aligned.
        unsafe { _mm512_stream_si512(base.add(i), vp) };
    }
    // Scalar tail for buffers not a multiple of 8 (unusual, but safe).
    for i in (n * 8)..buf.len() {
        // SAFETY: i < buf.len()
        unsafe { ptr::write_volatile(buf.as_mut_ptr().add(i), pattern) };
    }
    _mm_sfence();
}

/// Fill `buf` with sequential indices `[start, start+1, ...]` using NT stores.
///
/// Uses an incrementing vector to avoid reloading the expected-value vector
/// each iteration: initialize once, add 8 per step.
/// Falls back to scalar writes if not 64-byte aligned (same reasoning as `fill_nt`).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f")]
pub(crate) unsafe fn fill_nt_indexed(buf: &mut [u64], start: usize) {
    use std::arch::x86_64::{
        __m512i, _mm_sfence, _mm512_add_epi64, _mm512_set_epi64, _mm512_set1_epi64,
        _mm512_stream_si512,
    };
    if !(buf.as_ptr() as usize).is_multiple_of(64) {
        for (i, word) in buf.iter_mut().enumerate() {
            unsafe { ptr::write_volatile(std::ptr::from_mut::<u64>(word), (start + i) as u64) };
        }
        return;
    }
    // vcur = [start, start+1, ..., start+7]
    let mut vcur = _mm512_add_epi64(
        _mm512_set_epi64(7, 6, 5, 4, 3, 2, 1, 0),
        _mm512_set1_epi64(start as i64),
    );
    let vstep = _mm512_set1_epi64(8);
    let base = buf.as_mut_ptr().cast::<__m512i>();
    let n = buf.len() / 8;
    for i in 0..n {
        // SAFETY: base + i is within the buffer and 64-byte aligned.
        unsafe { _mm512_stream_si512(base.add(i), vcur) };
        vcur = _mm512_add_epi64(vcur, vstep);
    }
    for i in (n * 8)..buf.len() {
        // SAFETY: i < buf.len()
        unsafe { ptr::write_volatile(buf.as_mut_ptr().add(i), (start + i) as u64) };
    }
    _mm_sfence();
}

/// Verify that every word in `buf` equals `pattern` using AVX-512 comparison.
///
/// Compares 8 words at a time with a single `vpcmpeqq`. The slow path (building
/// `Failure` records) only executes when a mismatch is detected, which should be
/// almost never on healthy hardware. Non-volatile SIMD loads are safe here because
/// the write phase issued `_mm_sfence` and Rayon's join barrier has elapsed,
/// guaranteeing the writes are globally visible before any read in this phase.
///
/// `word_off` is the word index of `buf[0]` relative to the start of the full
/// buffer, used to compute absolute word indices and addresses in failure records.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f")]
pub(crate) unsafe fn verify_avx512(
    buf: &[u64],
    pattern: u64,
    base_addr: usize,
    word_off: usize,
) -> Vec<Failure> {
    use std::arch::x86_64::{
        __m512i, _mm512_cmpeq_epi64_mask, _mm512_loadu_si512, _mm512_set1_epi64,
    };
    let vp = _mm512_set1_epi64(pattern as i64);
    let base = buf.as_ptr().cast::<__m512i>();
    let n = buf.len() / 8;
    let mut failures = Vec::new();
    for i in 0..n {
        // Unaligned load works on any alignment (test buffers may not be 64B-aligned).
        // SAFETY: base + i is within the buffer.
        let v = unsafe { _mm512_loadu_si512(base.add(i)) };
        let mask = _mm512_cmpeq_epi64_mask(v, vp);
        if mask != 0xFF {
            let off = i * 8;
            for j in 0..8usize {
                if (mask >> j) & 1 == 0 {
                    let wi = word_off + off + j;
                    // SAFETY: off + j < n * 8 <= buf.len()
                    let actual = unsafe { *buf.get_unchecked(off + j) };
                    failures.push(Failure {
                        addr: base_addr + wi * 8,
                        expected: pattern,
                        actual,
                        word_index: wi,
                        phys_addr: None,
                    });
                }
            }
        }
    }
    for i in (n * 8)..buf.len() {
        // SAFETY: i < buf.len()
        let actual = unsafe { *buf.get_unchecked(i) };
        if actual != pattern {
            let wi = word_off + i;
            failures.push(Failure {
                addr: base_addr + wi * 8,
                expected: pattern,
                actual,
                word_index: wi,
                phys_addr: None,
            });
        }
    }
    failures
}

/// Verify that `buf[i] == (word_off + i) as u64` for all `i`, using AVX-512.
///
/// Maintains an incrementing expected-value vector in the same style as
/// `fill_nt_indexed` to avoid per-iteration recomputation.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f")]
pub(crate) unsafe fn verify_indexed_avx512(
    buf: &[u64],
    base_addr: usize,
    word_off: usize,
) -> Vec<Failure> {
    use std::arch::x86_64::{
        __m512i, _mm512_add_epi64, _mm512_cmpeq_epi64_mask, _mm512_loadu_si512, _mm512_set_epi64,
        _mm512_set1_epi64,
    };
    let mut vexp = _mm512_add_epi64(
        _mm512_set_epi64(7, 6, 5, 4, 3, 2, 1, 0),
        _mm512_set1_epi64(word_off as i64),
    );
    let vstep = _mm512_set1_epi64(8);
    let base = buf.as_ptr().cast::<__m512i>();
    let n = buf.len() / 8;
    let mut failures = Vec::new();
    for i in 0..n {
        // SAFETY: base + i is within the buffer.
        let v = unsafe { _mm512_loadu_si512(base.add(i)) };
        let mask = _mm512_cmpeq_epi64_mask(v, vexp);
        if mask != 0xFF {
            let off = i * 8;
            for j in 0..8usize {
                if (mask >> j) & 1 == 0 {
                    let wi = word_off + off + j;
                    // SAFETY: off + j < n * 8 <= buf.len()
                    let actual = unsafe { *buf.get_unchecked(off + j) };
                    failures.push(Failure {
                        addr: base_addr + wi * 8,
                        expected: wi as u64,
                        actual,
                        word_index: wi,
                        phys_addr: None,
                    });
                }
            }
        }
        vexp = _mm512_add_epi64(vexp, vstep);
    }
    for i in (n * 8)..buf.len() {
        let expected = (word_off + i) as u64;
        // SAFETY: i < buf.len()
        let actual = unsafe { *buf.get_unchecked(i) };
        if actual != expected {
            let wi = word_off + i;
            failures.push(Failure {
                addr: base_addr + wi * 8,
                expected,
                actual,
                word_index: wi,
                phys_addr: None,
            });
        }
    }
    failures
}

/// AVX-512 orchestration for constant fill-and-verify.
#[cfg(target_arch = "x86_64")]
pub(crate) fn fill_verify_constant(
    buf: &mut [u64],
    pattern: u64,
    parallel: bool,
    on_activity: &(dyn Fn(f64) + Sync),
) -> Vec<Failure> {
    let base_addr = buf.as_ptr() as usize;
    let total = buf.len();
    if parallel {
        buf.par_chunks_mut(CHUNK)
            .enumerate()
            .for_each(|(ci, chunk)| {
                // SAFETY: chunk starts at a 64-byte aligned address (mmap base is
                // page-aligned; every CHUNK * 8 byte boundary is 64-byte aligned).
                unsafe { fill_nt(chunk, pattern) };
                on_activity((ci * CHUNK) as f64 / total as f64);
            });
        // Rayon's join barrier ensures all NT stores and sfences have completed.
        buf.par_chunks(CHUNK)
            .enumerate()
            .flat_map_iter(|(ci, chunk)| {
                on_activity((ci * CHUNK) as f64 / total as f64);
                // SAFETY: same alignment argument as write side.
                unsafe { verify_avx512(chunk, pattern, base_addr, ci * CHUNK) }
            })
            .collect()
    } else {
        on_activity(0.0);
        unsafe {
            fill_nt(buf, pattern);
        }
        on_activity(0.5);
        let result = unsafe { verify_avx512(buf, pattern, base_addr, 0) };
        on_activity(1.0);
        result
    }
}

/// AVX-512 orchestration for indexed fill-and-verify.
#[cfg(target_arch = "x86_64")]
pub(crate) fn fill_verify_indexed(
    buf: &mut [u64],
    parallel: bool,
    on_activity: &(dyn Fn(f64) + Sync),
) -> Vec<Failure> {
    let base_addr = buf.as_ptr() as usize;
    let total = buf.len();
    if parallel {
        buf.par_chunks_mut(CHUNK)
            .enumerate()
            .for_each(|(ci, chunk)| {
                unsafe { fill_nt_indexed(chunk, ci * CHUNK) };
                on_activity((ci * CHUNK) as f64 / total as f64);
            });
        buf.par_chunks(CHUNK)
            .enumerate()
            .flat_map_iter(|(ci, chunk)| {
                on_activity((ci * CHUNK) as f64 / total as f64);
                unsafe { verify_indexed_avx512(chunk, base_addr, ci * CHUNK) }
            })
            .collect()
    } else {
        on_activity(0.0);
        unsafe {
            fill_nt_indexed(buf, 0);
        }
        on_activity(0.5);
        let result = unsafe { verify_indexed_avx512(buf, base_addr, 0) };
        on_activity(1.0);
        result
    }
}

#[cfg(test)]
#[cfg(target_arch = "x86_64")]
mod tests {
    use assert2::{assert, check};

    use super::*;

    #[test]
    fn fill_nt_and_verify_round_trip() {
        if !avx512_available() {
            return;
        }
        // Heap-allocated buffer -- tests the unaligned fallback path
        let mut buf = vec![0u64; 256];
        let pattern = 0xDEAD_BEEF_CAFE_BABEu64;
        unsafe { fill_nt(&mut buf, pattern) };
        let failures = unsafe { verify_avx512(&buf, pattern, buf.as_ptr() as usize, 0) };
        assert!(failures.is_empty());
    }

    #[test]
    fn fill_nt_indexed_and_verify_round_trip() {
        if !avx512_available() {
            return;
        }
        let mut buf = vec![0u64; 256];
        unsafe { fill_nt_indexed(&mut buf, 0) };
        let failures = unsafe { verify_indexed_avx512(&buf, buf.as_ptr() as usize, 0) };
        assert!(failures.is_empty());
    }

    #[test]
    fn verify_detects_corruption() {
        if !avx512_available() {
            return;
        }
        let mut buf = vec![0u64; 256];
        let pattern = 0xAAAA_AAAA_AAAA_AAAAu64;
        unsafe { fill_nt(&mut buf, pattern) };
        buf[42] = 0xBBBB_BBBB_BBBB_BBBBu64;
        let failures = unsafe { verify_avx512(&buf, pattern, buf.as_ptr() as usize, 0) };
        assert!(failures.len() == 1);
        check!(failures[0].word_index == 42);
        check!(failures[0].actual == 0xBBBB_BBBB_BBBB_BBBBu64);
        check!(failures[0].expected == pattern);
    }

    #[test]
    fn verify_indexed_detects_corruption() {
        if !avx512_available() {
            return;
        }
        let mut buf = vec![0u64; 256];
        unsafe { fill_nt_indexed(&mut buf, 0) };
        buf[10] = 0xFFFF;
        let failures = unsafe { verify_indexed_avx512(&buf, buf.as_ptr() as usize, 0) };
        assert!(failures.len() == 1);
        check!(failures[0].word_index == 10);
        check!(failures[0].expected == 10);
    }

    #[test]
    fn fill_nt_indexed_with_offset() {
        if !avx512_available() {
            return;
        }
        let mut buf = vec![0u64; 64];
        let start = 100;
        unsafe { fill_nt_indexed(&mut buf, start) };
        for (i, &val) in buf.iter().enumerate() {
            check!(val == (start + i) as u64, "mismatch at index {i}");
        }
    }

    #[test]
    fn fill_nt_scalar_tail() {
        if !avx512_available() {
            return;
        }
        // Buffer size not a multiple of 8 -- tests the scalar tail path
        let mut buf = vec![0u64; 13];
        let pattern = 0x1234_5678_9ABC_DEF0u64;
        unsafe { fill_nt(&mut buf, pattern) };
        for (i, &val) in buf.iter().enumerate() {
            check!(val == pattern, "mismatch at index {i}");
        }
    }

    #[test]
    fn verify_multiple_corruptions_different_lanes() {
        if !avx512_available() {
            return;
        }
        let mut buf = vec![0u64; 256];
        let pattern = 0xAAAA_AAAA_AAAA_AAAAu64;
        unsafe { fill_nt(&mut buf, pattern) };
        // Corrupt words in different 8-word SIMD lanes
        buf[3] = 0; // lane 0, word 3
        buf[12] = 0; // lane 1, word 4
        buf[255] = 0; // last word
        let failures = unsafe { verify_avx512(&buf, pattern, buf.as_ptr() as usize, 0) };
        assert!(failures.len() == 3);
        check!(failures[0].word_index == 3);
        check!(failures[1].word_index == 12);
        check!(failures[2].word_index == 255);
    }

    #[test]
    fn verify_corruption_at_lane_boundaries() {
        if !avx512_available() {
            return;
        }
        let mut buf = vec![0u64; 32];
        let pattern = 0xFFFF_FFFF_FFFF_FFFFu64;
        unsafe { fill_nt(&mut buf, pattern) };
        // First and last word of first SIMD lane
        buf[0] = 0;
        buf[7] = 0;
        // First word of second lane
        buf[8] = 0;
        let failures = unsafe { verify_avx512(&buf, pattern, buf.as_ptr() as usize, 0) };
        assert!(failures.len() == 3);
        check!(failures[0].word_index == 0);
        check!(failures[1].word_index == 7);
        check!(failures[2].word_index == 8);
    }

    #[test]
    fn verify_indexed_multiple_corruptions() {
        if !avx512_available() {
            return;
        }
        let mut buf = vec![0u64; 64];
        unsafe { fill_nt_indexed(&mut buf, 0) };
        buf[0] = 999;
        buf[63] = 999;
        let failures = unsafe { verify_indexed_avx512(&buf, buf.as_ptr() as usize, 0) };
        assert!(failures.len() == 2);
        check!(failures[0].word_index == 0);
        check!(failures[0].expected == 0);
        check!(failures[1].word_index == 63);
        check!(failures[1].expected == 63);
    }

    #[test]
    fn fill_nt_indexed_scalar_tail() {
        if !avx512_available() {
            return;
        }
        let mut buf = vec![0u64; 11];
        unsafe { fill_nt_indexed(&mut buf, 50) };
        for (i, &val) in buf.iter().enumerate() {
            check!(val == (50 + i) as u64, "mismatch at index {i}");
        }
    }

    #[test]
    fn verify_scalar_tail_corruption() {
        if !avx512_available() {
            return;
        }
        // 11 words: 8 in SIMD lane + 3 in scalar tail
        let mut buf = vec![0u64; 11];
        let pattern = 0x5555_5555_5555_5555u64;
        unsafe { fill_nt(&mut buf, pattern) };
        buf[9] = 0; // in scalar tail
        let failures = unsafe { verify_avx512(&buf, pattern, buf.as_ptr() as usize, 0) };
        assert!(failures.len() == 1);
        check!(failures[0].word_index == 9);
    }

    #[test]
    fn verify_indexed_scalar_tail_corruption() {
        if !avx512_available() {
            return;
        }
        let mut buf = vec![0u64; 11];
        unsafe { fill_nt_indexed(&mut buf, 0) };
        buf[10] = 999; // last word, in scalar tail
        let failures = unsafe { verify_indexed_avx512(&buf, buf.as_ptr() as usize, 0) };
        assert!(failures.len() == 1);
        check!(failures[0].word_index == 10);
        check!(failures[0].expected == 10);
    }

    #[test]
    fn fill_nt_empty_buffer() {
        if !avx512_available() {
            return;
        }
        let mut buf: Vec<u64> = vec![];
        unsafe { fill_nt(&mut buf, 0xFF) };
        // Should not panic
    }

    /// Return a slice of `size` words that is guaranteed NOT to be 64-byte aligned,
    /// forcing `fill_nt` / `fill_nt_indexed` onto the scalar fallback path.
    fn make_unaligned(backing: &mut Vec<u64>, size: usize) -> &mut [u64] {
        backing.resize(size + 1, 0u64);
        // shift by one word (8 bytes) if already aligned, to break 64-byte alignment
        let offset = usize::from((backing.as_ptr() as usize).is_multiple_of(64));
        &mut backing[offset..offset + size]
    }

    #[test]
    fn fill_nt_unaligned_fallback() {
        if !avx512_available() {
            return;
        }
        let mut backing = Vec::new();
        let buf = make_unaligned(&mut backing, 256);
        assert!(
            !(buf.as_ptr() as usize).is_multiple_of(64),
            "buffer must be unaligned"
        );
        let pattern = 0xCAFE_BABE_DEAD_BEEFu64;
        unsafe { fill_nt(buf, pattern) };
        for (i, &val) in buf.iter().enumerate() {
            check!(val == pattern, "mismatch at index {i}");
        }
    }

    #[test]
    fn fill_nt_indexed_unaligned_fallback() {
        if !avx512_available() {
            return;
        }
        let mut backing = Vec::new();
        let buf = make_unaligned(&mut backing, 64);
        assert!(
            !(buf.as_ptr() as usize).is_multiple_of(64),
            "buffer must be unaligned"
        );
        let start = 42;
        unsafe { fill_nt_indexed(buf, start) };
        for (i, &val) in buf.iter().enumerate() {
            check!(val == (start + i) as u64, "mismatch at index {i}");
        }
    }

    #[test]
    fn verify_with_word_offset() {
        if !avx512_available() {
            return;
        }
        let mut buf = vec![0u64; 16];
        let pattern = 0xAAAAu64;
        unsafe { fill_nt(&mut buf, pattern) };
        buf[5] = 0;
        // word_off=100 means buf[5] is word 105 globally
        let failures = unsafe { verify_avx512(&buf, pattern, buf.as_ptr() as usize, 100) };
        assert!(failures.len() == 1);
        check!(failures[0].word_index == 105);
    }
}
