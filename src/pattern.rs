use std::fmt;
use std::ptr;

use rayon::prelude::*;

/// A single test failure record.
#[derive(Debug, Clone)]
pub struct Failure {
    /// Virtual address of the failing word.
    pub addr: usize,
    /// Expected value.
    pub expected: u64,
    /// Actual value read back.
    pub actual: u64,
    /// Word index within the buffer.
    pub word_index: usize,
}

impl Failure {
    pub fn xor(&self) -> u64 {
        self.expected ^ self.actual
    }

    pub fn flipped_bits(&self) -> u32 {
        self.xor().count_ones()
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "FAIL  addr=0x{:016x}  expected=0x{:016x}  actual=0x{:016x}  xor=0x{:016x} ({} bit(s))",
            self.addr,
            self.expected,
            self.actual,
            self.xor(),
            self.flipped_bits(),
        )
    }
}

/// All supported test patterns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Pattern {
    SolidBits,
    WalkingOnes,
    WalkingZeros,
    Checkerboard,
    StuckAddress,
}

impl Pattern {
    pub const ALL: &[Pattern] = &[
        Pattern::SolidBits,
        Pattern::WalkingOnes,
        Pattern::WalkingZeros,
        Pattern::Checkerboard,
        Pattern::StuckAddress,
    ];

    /// Number of fill-and-verify sub-passes this pattern performs.
    /// Used to size the inner progress bar.
    pub fn sub_passes(&self) -> u64 {
        match self {
            Pattern::SolidBits => 2,
            Pattern::WalkingOnes => 64,
            Pattern::WalkingZeros => 64,
            Pattern::Checkerboard => 2,
            Pattern::StuckAddress => 1,
        }
    }
}

impl fmt::Display for Pattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Pattern::SolidBits => write!(f, "Solid Bits"),
            Pattern::WalkingOnes => write!(f, "Walking Ones"),
            Pattern::WalkingZeros => write!(f, "Walking Zeros"),
            Pattern::Checkerboard => write!(f, "Checkerboard"),
            Pattern::StuckAddress => write!(f, "Stuck Address"),
        }
    }
}

/// Run a test pattern on the given buffer, returning any failures found.
///
/// `buf_ptr` is the raw pointer to the start of the u64 buffer for address calculation.
/// `buf` is the slice of u64 words to test. All reads and writes use volatile
/// operations to prevent the compiler from optimizing away the memory accesses.
///
/// `parallel` enables multi-threaded write and verify phases via Rayon.
/// `on_subpass` is called after each internal fill-and-verify sub-pass, suitable
/// for driving a progress bar in the caller.
pub fn run_pattern(
    pattern: Pattern,
    buf_ptr: *const u8,
    buf: &mut [u64],
    parallel: bool,
    on_subpass: &mut impl FnMut(),
) -> Vec<Failure> {
    match pattern {
        Pattern::SolidBits => test_solid_bits(buf_ptr, buf, parallel, on_subpass),
        Pattern::WalkingOnes => test_walking_ones(buf_ptr, buf, parallel, on_subpass),
        Pattern::WalkingZeros => test_walking_zeros(buf_ptr, buf, parallel, on_subpass),
        Pattern::Checkerboard => test_checkerboard(buf_ptr, buf, parallel, on_subpass),
        Pattern::StuckAddress => test_stuck_address(buf_ptr, buf, parallel, on_subpass),
    }
}

// ─── chunk size ──────────────────────────────────────────────────────────────

/// Number of u64 words processed per Rayon task.
/// Must be a multiple of 8 (one AVX-512 register = 8 × u64 = 64 bytes) so that
/// every chunk boundary is 64-byte aligned and NT store / aligned load intrinsics
/// never straddle a chunk boundary.
const CHUNK: usize = 64 * 1024; // 64 K u64s = 512 KiB

// ─── AVX-512 helpers ─────────────────────────────────────────────────────────

/// Fill `buf` with `pattern` using AVX-512 non-temporal (streaming) stores.
///
/// NT stores write directly to DRAM, bypassing all CPU cache levels.
/// This avoids the read-for-ownership penalty of regular cached writes and
/// keeps caches warm for non-test data. Ends with `_mm_sfence` to flush
/// the write-combining buffers before returning.
///
/// Falls back to scalar volatile writes if the buffer is not 64-byte aligned
/// (e.g., heap-allocated test buffers). Production paths using mmap-backed
/// buffers are always page-aligned (≥ 4096 bytes) and always take the NT path.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f")]
unsafe fn fill_nt(buf: &mut [u64], pattern: u64) {
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

/// Fill `buf` with sequential indices `[start, start+1, …]` using NT stores.
///
/// Uses an incrementing vector to avoid reloading the expected-value vector
/// each iteration: initialize once, add 8 per step.
/// Falls back to scalar writes if not 64-byte aligned (same reasoning as `fill_nt`).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f")]
unsafe fn fill_nt_indexed(buf: &mut [u64], start: usize) {
    use std::arch::x86_64::*;
    if !(buf.as_ptr() as usize).is_multiple_of(64) {
        for (i, word) in buf.iter_mut().enumerate() {
            unsafe { ptr::write_volatile(word as *mut u64, (start + i) as u64) };
        }
        return;
    }
    // vcur = [start, start+1, …, start+7]
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
unsafe fn verify_avx512(
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
unsafe fn verify_indexed_avx512(buf: &[u64], base_addr: usize, word_off: usize) -> Vec<Failure> {
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
            });
        }
    }
    failures
}

/// Returns true if AVX-512F is available. With `target-cpu=native` this is a
/// compile-time constant and the dead branches are eliminated by LLVM.
#[inline(always)]
fn avx512_available() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        is_x86_feature_detected!("avx512f")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

// ─── fill_and_verify ─────────────────────────────────────────────────────────

/// Write `pattern` to every word, then verify. Calls `on_complete` once finished.
fn fill_and_verify(
    buf_ptr: *const u8,
    buf: &mut [u64],
    pattern: u64,
    parallel: bool,
    on_complete: &mut impl FnMut(),
) -> Vec<Failure> {
    let base_addr = buf.as_ptr() as usize;

    let failures = match (parallel, avx512_available()) {
        // AVX-512 parallel: NT stores per chunk + aligned SIMD verify per chunk.
        (true, true) => {
            buf.par_chunks_mut(CHUNK).for_each(|chunk| {
                // SAFETY: chunk starts at a 64-byte aligned address (mmap base is
                // page-aligned; every CHUNK * 8 byte boundary is 64-byte aligned).
                unsafe { fill_nt(chunk, pattern) };
            });
            // Rayon's join barrier after the above for_each ensures all NT stores
            // and their per-thread sfences have completed before reads begin.
            buf.par_chunks(CHUNK)
                .enumerate()
                .flat_map_iter(|(ci, chunk)| {
                    // SAFETY: same alignment argument as write side.
                    unsafe { verify_avx512(chunk, pattern, base_addr, ci * CHUNK) }
                })
                .collect()
        }
        // AVX-512 sequential: single NT fill + single SIMD verify.
        (false, true) => unsafe {
            fill_nt(buf, pattern);
            verify_avx512(buf, pattern, base_addr, 0)
        },
        // Fallback parallel: volatile writes + volatile reads (original behaviour).
        (true, false) => {
            buf.par_iter_mut().for_each(|word| {
                unsafe { ptr::write_volatile(word as *mut u64, pattern) };
            });
            buf.par_iter()
                .enumerate()
                .filter_map(|(i, word)| {
                    let actual = unsafe { ptr::read_volatile(word as *const u64) };
                    (actual != pattern).then(|| Failure {
                        addr: base_addr + i * 8,
                        expected: pattern,
                        actual,
                        word_index: i,
                    })
                })
                .collect()
        }
        // Fallback sequential: volatile writes + volatile reads (original behaviour).
        (false, false) => {
            let base = buf.as_ptr();
            for word in buf.iter_mut() {
                unsafe { ptr::write_volatile(word as *mut u64, pattern) };
            }
            buf.iter()
                .enumerate()
                .filter_map(|(i, word)| {
                    let actual = unsafe { ptr::read_volatile(word as *const u64) };
                    (actual != pattern).then(|| Failure {
                        addr: unsafe {
                            (base.add(i) as *const u8).offset_from(buf_ptr) as usize
                                + buf_ptr as usize
                        },
                        expected: pattern,
                        actual,
                        word_index: i,
                    })
                })
                .collect()
        }
    };

    on_complete();
    failures
}

// ─── pattern implementations ─────────────────────────────────────────────────

fn test_solid_bits(
    buf_ptr: *const u8,
    buf: &mut [u64],
    parallel: bool,
    on_subpass: &mut impl FnMut(),
) -> Vec<Failure> {
    let mut failures = Vec::new();
    failures.extend(fill_and_verify(
        buf_ptr,
        buf,
        0x0000_0000_0000_0000,
        parallel,
        on_subpass,
    ));
    failures.extend(fill_and_verify(
        buf_ptr,
        buf,
        0xFFFF_FFFF_FFFF_FFFF,
        parallel,
        on_subpass,
    ));
    failures
}

fn test_walking_ones(
    buf_ptr: *const u8,
    buf: &mut [u64],
    parallel: bool,
    on_subpass: &mut impl FnMut(),
) -> Vec<Failure> {
    let mut failures = Vec::new();
    for bit in 0..64 {
        let pattern = 1u64 << bit;
        failures.extend(fill_and_verify(buf_ptr, buf, pattern, parallel, on_subpass));
    }
    failures
}

fn test_walking_zeros(
    buf_ptr: *const u8,
    buf: &mut [u64],
    parallel: bool,
    on_subpass: &mut impl FnMut(),
) -> Vec<Failure> {
    let mut failures = Vec::new();
    for bit in 0..64 {
        let pattern = !(1u64 << bit);
        failures.extend(fill_and_verify(buf_ptr, buf, pattern, parallel, on_subpass));
    }
    failures
}

fn test_checkerboard(
    buf_ptr: *const u8,
    buf: &mut [u64],
    parallel: bool,
    on_subpass: &mut impl FnMut(),
) -> Vec<Failure> {
    let mut failures = Vec::new();
    failures.extend(fill_and_verify(
        buf_ptr,
        buf,
        0xAAAA_AAAA_AAAA_AAAA,
        parallel,
        on_subpass,
    ));
    failures.extend(fill_and_verify(
        buf_ptr,
        buf,
        0x5555_5555_5555_5555,
        parallel,
        on_subpass,
    ));
    failures
}

fn test_stuck_address(
    buf_ptr: *const u8,
    buf: &mut [u64],
    parallel: bool,
    on_subpass: &mut impl FnMut(),
) -> Vec<Failure> {
    let base_addr = buf.as_ptr() as usize;

    let failures = match (parallel, avx512_available()) {
        (true, true) => {
            buf.par_chunks_mut(CHUNK)
                .enumerate()
                .for_each(|(ci, chunk)| {
                    unsafe { fill_nt_indexed(chunk, ci * CHUNK) };
                });
            buf.par_chunks(CHUNK)
                .enumerate()
                .flat_map_iter(|(ci, chunk)| unsafe {
                    verify_indexed_avx512(chunk, base_addr, ci * CHUNK)
                })
                .collect()
        }
        (false, true) => unsafe {
            fill_nt_indexed(buf, 0);
            verify_indexed_avx512(buf, base_addr, 0)
        },
        (true, false) => {
            buf.par_iter_mut().enumerate().for_each(|(i, word)| {
                unsafe { ptr::write_volatile(word as *mut u64, i as u64) };
            });
            buf.par_iter()
                .enumerate()
                .filter_map(|(i, word)| {
                    let expected = i as u64;
                    let actual = unsafe { ptr::read_volatile(word as *const u64) };
                    (actual != expected).then(|| Failure {
                        addr: base_addr + i * 8,
                        expected,
                        actual,
                        word_index: i,
                    })
                })
                .collect()
        }
        (false, false) => {
            let base = buf.as_ptr();
            for (i, word) in buf.iter_mut().enumerate() {
                unsafe { ptr::write_volatile(word as *mut u64, i as u64) };
            }
            buf.iter()
                .enumerate()
                .filter_map(|(i, word)| {
                    let expected = i as u64;
                    let actual = unsafe { ptr::read_volatile(word as *const u64) };
                    (actual != expected).then(|| Failure {
                        addr: unsafe {
                            (base.add(i) as *const u8).offset_from(buf_ptr) as usize
                                + buf_ptr as usize
                        },
                        expected,
                        actual,
                        word_index: i,
                    })
                })
                .collect()
        }
    };

    on_subpass();
    failures
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a small test buffer on the heap (no mmap needed for unit tests).
    fn make_test_buf() -> Vec<u64> {
        vec![0u64; 1024]
    }

    #[test]
    fn solid_bits_no_failures_on_good_memory() {
        let mut buf = make_test_buf();
        let ptr = buf.as_ptr() as *const u8;
        let failures = test_solid_bits(ptr, &mut buf, false, &mut || {});
        assert!(failures.is_empty());
    }

    #[test]
    fn walking_ones_no_failures() {
        let mut buf = make_test_buf();
        let ptr = buf.as_ptr() as *const u8;
        let failures = test_walking_ones(ptr, &mut buf, false, &mut || {});
        assert!(failures.is_empty());
    }

    #[test]
    fn checkerboard_no_failures() {
        let mut buf = make_test_buf();
        let ptr = buf.as_ptr() as *const u8;
        let failures = test_checkerboard(ptr, &mut buf, false, &mut || {});
        assert!(failures.is_empty());
    }

    #[test]
    fn stuck_address_no_failures() {
        let mut buf = make_test_buf();
        let ptr = buf.as_ptr() as *const u8;
        let failures = test_stuck_address(ptr, &mut buf, false, &mut || {});
        assert!(failures.is_empty());
    }

    #[test]
    fn parallel_solid_bits_no_failures() {
        let mut buf = make_test_buf();
        let ptr = buf.as_ptr() as *const u8;
        let failures = test_solid_bits(ptr, &mut buf, true, &mut || {});
        assert!(failures.is_empty());
    }

    #[test]
    fn parallel_walking_ones_no_failures() {
        let mut buf = make_test_buf();
        let ptr = buf.as_ptr() as *const u8;
        let failures = test_walking_ones(ptr, &mut buf, true, &mut || {});
        assert!(failures.is_empty());
    }

    #[test]
    fn parallel_stuck_address_no_failures() {
        let mut buf = make_test_buf();
        let ptr = buf.as_ptr() as *const u8;
        let failures = test_stuck_address(ptr, &mut buf, true, &mut || {});
        assert!(failures.is_empty());
    }

    #[test]
    fn failure_display_format() {
        let f = Failure {
            addr: 0x1000,
            expected: 0xAAAA_AAAA_AAAA_AAAA,
            actual: 0xAAAA_AAAA_AABA_AAAA,
            word_index: 0,
        };
        assert_eq!(f.flipped_bits(), 1);
        let s = f.to_string();
        assert!(s.contains("FAIL"));
        assert!(s.contains("1 bit(s)"));
    }
}
