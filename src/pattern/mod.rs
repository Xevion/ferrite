use std::fmt;
use std::ptr;

use rayon::prelude::*;

use crate::Failure;
#[cfg(target_arch = "x86_64")]
use crate::simd::{
    CHUNK, avx512_available, fill_nt, fill_nt_indexed, verify_avx512, verify_indexed_avx512,
};

mod checkerboard;
mod solid;
mod stuck_address;
mod walking;

/// Chunk size (in u64 words) for activity reporting in non-SIMD paths.
/// Matches `simd::CHUNK` so activity granularity is consistent regardless
/// of whether AVX-512 is available.
const REPORT_CHUNK: usize = 64 * 1024; // 512 KiB

/// All supported test patterns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, strum::EnumCount)]
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
    #[must_use]
    pub fn sub_passes(&self) -> u64 {
        match self {
            Pattern::SolidBits | Pattern::Checkerboard => 2,
            Pattern::WalkingOnes | Pattern::WalkingZeros => 64,
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
/// All reads and writes use volatile operations to prevent the compiler from
/// optimizing away the memory accesses.
///
/// `parallel` enables multi-threaded write and verify phases via Rayon.
/// `on_subpass` is called after each internal fill-and-verify sub-pass, suitable
/// for driving a progress bar in the caller.
/// `on_activity` is called from worker threads with a position (0.0..1.0) within
/// the buffer, suitable for driving activity heatmaps.
pub fn run_pattern(
    pattern: Pattern,
    buf: &mut [u64],
    parallel: bool,
    on_subpass: &mut impl FnMut(),
    on_activity: &(dyn Fn(f64) + Sync),
) -> Vec<Failure> {
    match pattern {
        Pattern::SolidBits => solid::run(buf, parallel, on_subpass, on_activity),
        Pattern::WalkingOnes => walking::run_ones(buf, parallel, on_subpass, on_activity),
        Pattern::WalkingZeros => walking::run_zeros(buf, parallel, on_subpass, on_activity),
        Pattern::Checkerboard => checkerboard::run(buf, parallel, on_subpass, on_activity),
        Pattern::StuckAddress => stuck_address::run(buf, parallel, on_subpass, on_activity),
    }
}

/// Scalar fill: write `pattern` to every word using volatile stores.
pub(crate) fn scalar_fill_constant(buf: &mut [u64], pattern: u64) {
    for word in buf.iter_mut() {
        unsafe { ptr::write_volatile(std::ptr::from_mut::<u64>(word), pattern) };
    }
}

/// Scalar verify: read every word and report mismatches against `pattern`.
///
/// `word_start` is added to each failure's `word_index` so callers can pass a
/// chunk-global offset and get back globally-correct indices without post-fixup.
pub(crate) fn scalar_verify_constant(
    buf: &[u64],
    pattern: u64,
    base_addr: usize,
    word_start: usize,
) -> Vec<Failure> {
    buf.iter()
        .enumerate()
        .filter_map(|(i, word)| {
            let actual = unsafe { ptr::read_volatile(std::ptr::from_ref::<u64>(word)) };
            (actual != pattern).then(|| Failure {
                addr: base_addr + i * 8,
                expected: pattern,
                actual,
                word_index: word_start + i,
                phys_addr: None,
            })
        })
        .collect()
}

/// Scalar fill: write each word's index as its value using volatile stores.
pub(crate) fn scalar_fill_indexed(buf: &mut [u64], start: usize) {
    for (i, word) in buf.iter_mut().enumerate() {
        unsafe { ptr::write_volatile(std::ptr::from_mut::<u64>(word), (start + i) as u64) };
    }
}

/// Scalar verify: read every word and report mismatches against its expected index.
pub(crate) fn scalar_verify_indexed(buf: &[u64], base_addr: usize, start: usize) -> Vec<Failure> {
    buf.iter()
        .enumerate()
        .filter_map(|(i, word)| {
            let expected = (start + i) as u64;
            let actual = unsafe { ptr::read_volatile(std::ptr::from_ref::<u64>(word)) };
            (actual != expected).then(|| Failure {
                addr: base_addr + i * 8,
                expected,
                actual,
                word_index: start + i,
                phys_addr: None,
            })
        })
        .collect()
}

/// Fill every word with `pattern`, then verify. Returns any mismatches.
pub(super) fn fill_verify_constant(
    buf: &mut [u64],
    pattern: u64,
    parallel: bool,
    on_activity: &(dyn Fn(f64) + Sync),
) -> Vec<Failure> {
    let base_addr = buf.as_ptr() as usize;
    let total = buf.len();

    #[cfg(target_arch = "x86_64")]
    if avx512_available() {
        return if parallel {
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
        };
    }

    if parallel {
        let total = buf.len();
        buf.par_chunks_mut(REPORT_CHUNK)
            .enumerate()
            .for_each(|(ci, chunk)| {
                scalar_fill_constant(chunk, pattern);
                on_activity((ci * REPORT_CHUNK) as f64 / total as f64);
            });
        buf.par_chunks(REPORT_CHUNK)
            .enumerate()
            .flat_map_iter(|(ci, chunk)| {
                let chunk_start = ci * REPORT_CHUNK;
                on_activity(chunk_start as f64 / total as f64);
                scalar_verify_constant(chunk, pattern, base_addr + chunk_start * 8, chunk_start)
            })
            .collect()
    } else {
        for (ci, chunk) in buf.chunks_mut(REPORT_CHUNK).enumerate() {
            scalar_fill_constant(chunk, pattern);
            on_activity((ci * REPORT_CHUNK) as f64 / total as f64);
        }
        scalar_verify_constant(buf, pattern, base_addr, 0)
    }
}

/// Fill every word with its index, then verify. Returns any mismatches.
pub(super) fn fill_verify_indexed(
    buf: &mut [u64],
    parallel: bool,
    on_activity: &(dyn Fn(f64) + Sync),
) -> Vec<Failure> {
    let base_addr = buf.as_ptr() as usize;
    let total = buf.len();

    #[cfg(target_arch = "x86_64")]
    if avx512_available() {
        return if parallel {
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
        };
    }

    if parallel {
        let total = buf.len();
        buf.par_chunks_mut(REPORT_CHUNK)
            .enumerate()
            .for_each(|(ci, chunk)| {
                let chunk_start = ci * REPORT_CHUNK;
                scalar_fill_indexed(chunk, chunk_start);
                on_activity(chunk_start as f64 / total as f64);
            });
        buf.par_chunks(REPORT_CHUNK)
            .enumerate()
            .flat_map_iter(|(ci, chunk)| {
                let chunk_start = ci * REPORT_CHUNK;
                on_activity(chunk_start as f64 / total as f64);
                scalar_verify_indexed(chunk, base_addr + chunk_start * 8, chunk_start)
            })
            .collect()
    } else {
        for (ci, chunk) in buf.chunks_mut(REPORT_CHUNK).enumerate() {
            let chunk_start = ci * REPORT_CHUNK;
            scalar_fill_indexed(chunk, chunk_start);
            on_activity(chunk_start as f64 / total as f64);
        }
        scalar_verify_indexed(buf, base_addr, 0)
    }
}

/// Fill with `pattern`, verify, then call `on_complete`.
pub(super) fn fill_and_verify(
    buf: &mut [u64],
    pattern: u64,
    parallel: bool,
    on_complete: &mut impl FnMut(),
    on_activity: &(dyn Fn(f64) + Sync),
) -> Vec<Failure> {
    let failures = fill_verify_constant(buf, pattern, parallel, on_activity);
    on_complete();
    failures
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};
    use strum::EnumCount as _;

    use super::*;

    /// Create a small test buffer on the heap (no mmap needed for unit tests).
    fn make_test_buf() -> Vec<u64> {
        vec![0u64; 1024]
    }

    static NOOP_ACTIVITY: fn(f64) = |_| {};

    /// Parametrized clean-memory tests — one case per pattern × serial/parallel.
    /// New patterns added to `Pattern::ALL` are automatically covered here.
    mod clean_memory {
        use assert2::assert;
        use rstest::rstest;

        use super::*;

        #[rstest]
        #[case(Pattern::SolidBits)]
        #[case(Pattern::WalkingOnes)]
        #[case(Pattern::WalkingZeros)]
        #[case(Pattern::Checkerboard)]
        #[case(Pattern::StuckAddress)]
        fn serial(#[case] pattern: Pattern) {
            let mut buf = make_test_buf();
            let failures = run_pattern(pattern, &mut buf, false, &mut || {}, &NOOP_ACTIVITY);
            assert!(
                failures.is_empty(),
                "pattern {pattern} had failures on clean memory"
            );
        }

        #[rstest]
        #[case(Pattern::SolidBits)]
        #[case(Pattern::WalkingOnes)]
        #[case(Pattern::WalkingZeros)]
        #[case(Pattern::Checkerboard)]
        #[case(Pattern::StuckAddress)]
        fn parallel(#[case] pattern: Pattern) {
            let mut buf = make_test_buf();
            let failures = run_pattern(pattern, &mut buf, true, &mut || {}, &NOOP_ACTIVITY);
            assert!(
                failures.is_empty(),
                "pattern {pattern} had failures on clean memory (parallel)"
            );
        }
    }

    #[test]
    fn fill_verify_constant_clean_memory() {
        let mut buf = make_test_buf();
        // fill_verify_constant overwrites everything, so memory starting corrupt is fine
        let failures = fill_verify_constant(&mut buf, 0xFFFF_FFFF_FFFF_FFFF, false, &NOOP_ACTIVITY);
        assert!(failures.is_empty());
    }

    #[test]
    fn subpass_callback_fires() {
        let mut buf = make_test_buf();
        let mut count = 0u32;
        solid::run(&mut buf, false, &mut || count += 1, &NOOP_ACTIVITY);
        check!(count == 2); // solid_bits has 2 sub-passes
    }

    #[test]
    fn failure_display_format() {
        let f = Failure {
            addr: 0x1000,
            expected: 0xAAAA_AAAA_AAAA_AAAA,
            actual: 0xAAAA_AAAA_AABA_AAAA,
            word_index: 0,
            phys_addr: None,
        };
        check!(f.flipped_bits() == 1);
        let s = f.to_string();
        assert!(s.contains("FAIL"));
        assert!(s.contains("1 bit(s)"));
    }

    #[test]
    fn pattern_all_covers_every_variant() {
        assert!(
            Pattern::ALL.len() == Pattern::COUNT,
            "Pattern::ALL is missing variants — update it when adding new patterns"
        );
    }

    mod corruption {
        use assert2::{assert, check};

        use super::*;

        /// Fill every word with `pattern` using volatile writes.
        fn fill_const(buf: &mut [u64], pattern: u64) {
            for word in buf.iter_mut() {
                unsafe { ptr::write_volatile(std::ptr::from_mut::<u64>(word), pattern) };
            }
        }

        /// Fill every word with its index using volatile writes.
        fn fill_indexed(buf: &mut [u64]) {
            for (i, word) in buf.iter_mut().enumerate() {
                unsafe { ptr::write_volatile(std::ptr::from_mut::<u64>(word), i as u64) };
            }
        }

        /// Verify every word equals `pattern` using volatile reads.
        fn verify_const(buf: &[u64], pattern: u64) -> Vec<Failure> {
            let base_addr = buf.as_ptr() as usize;
            buf.iter()
                .enumerate()
                .filter_map(|(i, word)| {
                    let actual = unsafe { ptr::read_volatile(std::ptr::from_ref::<u64>(word)) };
                    (actual != pattern).then(|| Failure {
                        addr: base_addr + i * 8,
                        expected: pattern,
                        actual,
                        word_index: i,
                        phys_addr: None,
                    })
                })
                .collect()
        }

        /// Verify every word equals its index using volatile reads.
        fn verify_indexed(buf: &[u64]) -> Vec<Failure> {
            let base_addr = buf.as_ptr() as usize;
            buf.iter()
                .enumerate()
                .filter_map(|(i, word)| {
                    let expected = i as u64;
                    let actual = unsafe { ptr::read_volatile(std::ptr::from_ref::<u64>(word)) };
                    (actual != expected).then(|| Failure {
                        addr: base_addr + i * 8,
                        expected,
                        actual,
                        word_index: i,
                        phys_addr: None,
                    })
                })
                .collect()
        }

        #[test]
        fn constant_detects_single_corruption_serial() {
            let mut buf = vec![0u64; 1024];
            let pattern = 0xAAAA_AAAA_AAAA_AAAAu64;
            fill_const(&mut buf, pattern);
            buf[42] = 0xBBBB_BBBB_BBBB_BBBBu64;
            let failures = verify_const(&buf, pattern);
            assert!(failures.len() == 1);
            check!(failures[0].word_index == 42);
            check!(failures[0].actual == 0xBBBB_BBBB_BBBB_BBBBu64);
        }

        #[test]
        fn constant_detects_multiple_corruptions() {
            let mut buf = vec![0u64; 1024];
            let pattern = 0xFFFF_FFFF_FFFF_FFFFu64;
            fill_const(&mut buf, pattern);
            buf[0] = 0;
            buf[511] = 0;
            buf[1023] = 0;
            let failures = verify_const(&buf, pattern);
            assert!(failures.len() == 3);
            check!(failures[0].word_index == 0);
            check!(failures[1].word_index == 511);
            check!(failures[2].word_index == 1023);
        }

        #[test]
        fn indexed_detects_corruption_serial() {
            let mut buf = vec![0u64; 256];
            fill_indexed(&mut buf);
            buf[100] = 0xDEAD;
            let failures = verify_indexed(&buf);
            assert!(failures.len() == 1);
            check!(failures[0].word_index == 100);
            check!(failures[0].expected == 100);
            check!(failures[0].actual == 0xDEAD);
        }

        #[test]
        fn fill_verify_constant_single_word() {
            let mut buf = vec![0u64; 1];
            let failures = fill_verify_constant(&mut buf, 0xDEAD_BEEF, false, &NOOP_ACTIVITY);
            assert!(failures.is_empty());
        }

        #[test]
        fn fill_verify_indexed_single_word() {
            let mut buf = vec![0u64; 1];
            let failures = fill_verify_indexed(&mut buf, false, &NOOP_ACTIVITY);
            assert!(failures.is_empty());
        }

        #[test]
        fn fill_verify_constant_parallel_clean() {
            let mut buf = vec![0u64; 4096];
            let failures =
                fill_verify_constant(&mut buf, 0x5555_5555_5555_5555, true, &NOOP_ACTIVITY);
            assert!(failures.is_empty());
        }

        #[test]
        fn fill_verify_indexed_parallel_clean() {
            let mut buf = vec![0u64; 4096];
            let failures = fill_verify_indexed(&mut buf, true, &NOOP_ACTIVITY);
            assert!(failures.is_empty());
        }

        #[test]
        fn fill_verify_constant_empty_buffer() {
            let mut buf: Vec<u64> = vec![];
            let failures = fill_verify_constant(&mut buf, 0xFF, false, &NOOP_ACTIVITY);
            assert!(failures.is_empty());
        }

        #[test]
        fn fill_verify_indexed_empty_buffer() {
            let mut buf: Vec<u64> = vec![];
            let failures = fill_verify_indexed(&mut buf, false, &NOOP_ACTIVITY);
            assert!(failures.is_empty());
        }

        #[test]
        fn fill_and_verify_calls_on_complete() {
            let mut buf = vec![0u64; 64];
            let mut called = false;
            let _ = fill_and_verify(&mut buf, 0xAA, false, &mut || called = true, &NOOP_ACTIVITY);
            assert!(called);
        }

        #[test]
        fn non_chunk_multiple_buffer_size() {
            // 1000 is not a multiple of REPORT_CHUNK (64*1024), testing partial chunk handling
            let mut buf = vec![0u64; 1000];
            let failures =
                fill_verify_constant(&mut buf, 0x1234_5678_9ABC_DEF0, false, &NOOP_ACTIVITY);
            assert!(failures.is_empty());

            let failures = fill_verify_indexed(&mut buf, false, &NOOP_ACTIVITY);
            assert!(failures.is_empty());
        }
    }

    mod scalar_helpers {
        use assert2::{assert, check};

        use super::*;

        #[test]
        fn constant_round_trip() {
            let mut buf = vec![0u64; 256];
            scalar_fill_constant(&mut buf, 0xAAAA_AAAA_AAAA_AAAAu64);
            let base = buf.as_ptr() as usize;
            let failures = scalar_verify_constant(&buf, 0xAAAA_AAAA_AAAA_AAAAu64, base, 0);
            assert!(failures.is_empty());
        }

        #[test]
        fn constant_detects_single_corruption() {
            let mut buf = vec![0u64; 256];
            let pattern = 0xFFFF_FFFF_FFFF_FFFFu64;
            scalar_fill_constant(&mut buf, pattern);
            buf[10] = 0;
            let base = buf.as_ptr() as usize;
            let failures = scalar_verify_constant(&buf, pattern, base, 0);
            assert!(failures.len() == 1);
            check!(failures[0].word_index == 10);
            check!(failures[0].addr == base + 10 * 8);
            check!(failures[0].expected == pattern);
            check!(failures[0].actual == 0);
        }

        #[test]
        fn constant_detects_multiple_corruptions() {
            let mut buf = vec![0u64; 256];
            let pattern = 0x5555_5555_5555_5555u64;
            scalar_fill_constant(&mut buf, pattern);
            buf[0] = 1;
            buf[127] = 2;
            buf[255] = 3;
            let base = buf.as_ptr() as usize;
            let failures = scalar_verify_constant(&buf, pattern, base, 0);
            assert!(failures.len() == 3);
            check!(failures[0].word_index == 0);
            check!(failures[1].word_index == 127);
            check!(failures[2].word_index == 255);
        }

        #[test]
        fn constant_empty_buffer() {
            let mut buf: Vec<u64> = vec![];
            scalar_fill_constant(&mut buf, 0xFF);
            let failures = scalar_verify_constant(&buf, 0xFF, 0, 0);
            assert!(failures.is_empty());
        }

        #[test]
        fn indexed_round_trip() {
            let mut buf = vec![0u64; 256];
            scalar_fill_indexed(&mut buf, 0);
            let base = buf.as_ptr() as usize;
            let failures = scalar_verify_indexed(&buf, base, 0);
            assert!(failures.is_empty());
        }

        #[test]
        fn indexed_round_trip_with_offset() {
            // start=100: buf[i] should equal 100+i
            let mut buf = vec![0u64; 64];
            scalar_fill_indexed(&mut buf, 100);
            for (i, &val) in buf.iter().enumerate() {
                check!(val == (100 + i) as u64, "mismatch at i={i}");
            }
            let base = buf.as_ptr() as usize;
            let failures = scalar_verify_indexed(&buf, base, 100);
            assert!(failures.is_empty());
        }

        #[test]
        fn indexed_detects_single_corruption() {
            let mut buf = vec![0u64; 256];
            scalar_fill_indexed(&mut buf, 0);
            buf[50] = 0xDEAD;
            let base = buf.as_ptr() as usize;
            let failures = scalar_verify_indexed(&buf, base, 0);
            assert!(failures.len() == 1);
            check!(failures[0].word_index == 50);
            check!(failures[0].expected == 50);
            check!(failures[0].actual == 0xDEAD);
            check!(failures[0].addr == base + 50 * 8);
        }

        #[test]
        fn indexed_detects_multiple_corruptions() {
            let mut buf = vec![0u64; 64];
            scalar_fill_indexed(&mut buf, 0);
            buf[0] = 999;
            buf[63] = 999;
            let base = buf.as_ptr() as usize;
            let failures = scalar_verify_indexed(&buf, base, 0);
            assert!(failures.len() == 2);
            check!(failures[0].word_index == 0);
            check!(failures[1].word_index == 63);
        }

        #[test]
        fn indexed_empty_buffer() {
            let mut buf: Vec<u64> = vec![];
            scalar_fill_indexed(&mut buf, 0);
            let failures = scalar_verify_indexed(&buf, 0, 0);
            assert!(failures.is_empty());
        }
    }
}
