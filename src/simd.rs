#[cfg(target_arch = "x86_64")]
use std::ptr;

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
#[inline(always)]
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
    use std::arch::x86_64::*;
    if !(buf.as_ptr() as usize).is_multiple_of(64) {
        // Unaligned fallback: NT stores require 64-byte alignment.
        for word in buf.iter_mut() {
            unsafe { ptr::write_volatile(word as *mut u64, pattern) };
        }
        return;
    }
    let vp = _mm512_set1_epi64(pattern as i64);
    let base = buf.as_mut_ptr() as *mut __m512i;
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
    use std::arch::x86_64::*;
    if !(buf.as_ptr() as usize).is_multiple_of(64) {
        for (i, word) in buf.iter_mut().enumerate() {
            unsafe { ptr::write_volatile(word as *mut u64, (start + i) as u64) };
        }
        return;
    }
    // vcur = [start, start+1, ..., start+7]
    let mut vcur = _mm512_add_epi64(
        _mm512_set_epi64(7, 6, 5, 4, 3, 2, 1, 0),
        _mm512_set1_epi64(start as i64),
    );
    let vstep = _mm512_set1_epi64(8);
    let base = buf.as_mut_ptr() as *mut __m512i;
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
    use std::arch::x86_64::*;
    let vp = _mm512_set1_epi64(pattern as i64);
    let base = buf.as_ptr() as *const __m512i;
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
    use std::arch::x86_64::*;
    let mut vexp = _mm512_add_epi64(
        _mm512_set_epi64(7, 6, 5, 4, 3, 2, 1, 0),
        _mm512_set1_epi64(word_off as i64),
    );
    let vstep = _mm512_set1_epi64(8);
    let base = buf.as_ptr() as *const __m512i;
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
