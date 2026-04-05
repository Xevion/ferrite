use std::ptr;

use rayon::prelude::*;

use crate::Failure;

/// Chunk size (in u64 words) for activity reporting in scalar paths.
/// Matches `avx512::CHUNK` so activity granularity is consistent regardless
/// of whether AVX-512 is available.
const REPORT_CHUNK: usize = 64 * 1024; // 512 KiB

/// Scalar fill: write `pattern` to every word using volatile stores.
pub(crate) fn fill_constant(buf: &mut [u64], pattern: u64) {
    for word in buf.iter_mut() {
        unsafe { ptr::write_volatile(std::ptr::from_mut::<u64>(word), pattern) };
    }
}

/// Scalar verify: read every word and report mismatches against `pattern`.
///
/// `word_start` is added to each failure's `word_index` so callers can pass a
/// chunk-global offset and get back globally-correct indices without post-fixup.
pub(crate) fn verify_constant(
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
pub(crate) fn fill_indexed(buf: &mut [u64], start: usize) {
    for (i, word) in buf.iter_mut().enumerate() {
        unsafe { ptr::write_volatile(std::ptr::from_mut::<u64>(word), (start + i) as u64) };
    }
}

/// Scalar verify: read every word and report mismatches against its expected index.
pub(crate) fn verify_indexed(buf: &[u64], base_addr: usize, start: usize) -> Vec<Failure> {
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

/// Scalar orchestration for constant fill-and-verify.
pub(crate) fn fill_verify_constant(
    buf: &mut [u64],
    pattern: u64,
    parallel: bool,
    on_activity: &(dyn Fn(f64) + Sync),
) -> Vec<Failure> {
    let base_addr = buf.as_ptr() as usize;
    let total = buf.len();
    if parallel {
        buf.par_chunks_mut(REPORT_CHUNK)
            .enumerate()
            .for_each(|(ci, chunk)| {
                fill_constant(chunk, pattern);
                on_activity((ci * REPORT_CHUNK) as f64 / total as f64);
            });
        buf.par_chunks(REPORT_CHUNK)
            .enumerate()
            .flat_map_iter(|(ci, chunk)| {
                let chunk_start = ci * REPORT_CHUNK;
                on_activity(chunk_start as f64 / total as f64);
                verify_constant(chunk, pattern, base_addr + chunk_start * 8, chunk_start)
            })
            .collect()
    } else {
        for (ci, chunk) in buf.chunks_mut(REPORT_CHUNK).enumerate() {
            fill_constant(chunk, pattern);
            on_activity((ci * REPORT_CHUNK) as f64 / total as f64);
        }
        verify_constant(buf, pattern, base_addr, 0)
    }
}

/// Scalar orchestration for indexed fill-and-verify.
pub(crate) fn fill_verify_indexed(
    buf: &mut [u64],
    parallel: bool,
    on_activity: &(dyn Fn(f64) + Sync),
) -> Vec<Failure> {
    let base_addr = buf.as_ptr() as usize;
    let total = buf.len();
    if parallel {
        buf.par_chunks_mut(REPORT_CHUNK)
            .enumerate()
            .for_each(|(ci, chunk)| {
                let chunk_start = ci * REPORT_CHUNK;
                fill_indexed(chunk, chunk_start);
                on_activity(chunk_start as f64 / total as f64);
            });
        buf.par_chunks(REPORT_CHUNK)
            .enumerate()
            .flat_map_iter(|(ci, chunk)| {
                let chunk_start = ci * REPORT_CHUNK;
                on_activity(chunk_start as f64 / total as f64);
                verify_indexed(chunk, base_addr + chunk_start * 8, chunk_start)
            })
            .collect()
    } else {
        for (ci, chunk) in buf.chunks_mut(REPORT_CHUNK).enumerate() {
            let chunk_start = ci * REPORT_CHUNK;
            fill_indexed(chunk, chunk_start);
            on_activity(chunk_start as f64 / total as f64);
        }
        verify_indexed(buf, base_addr, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod primitives {
        use assert2::{assert, check};

        use super::*;

        #[test]
        fn constant_round_trip() {
            let mut buf = vec![0u64; 256];
            fill_constant(&mut buf, 0xAAAA_AAAA_AAAA_AAAAu64);
            let base = buf.as_ptr() as usize;
            let failures = verify_constant(&buf, 0xAAAA_AAAA_AAAA_AAAAu64, base, 0);
            assert!(failures.is_empty());
        }

        #[test]
        fn constant_detects_single_corruption() {
            let mut buf = vec![0u64; 256];
            let pattern = 0xFFFF_FFFF_FFFF_FFFFu64;
            fill_constant(&mut buf, pattern);
            buf[10] = 0;
            let base = buf.as_ptr() as usize;
            let failures = verify_constant(&buf, pattern, base, 0);
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
            fill_constant(&mut buf, pattern);
            buf[0] = 1;
            buf[127] = 2;
            buf[255] = 3;
            let base = buf.as_ptr() as usize;
            let failures = verify_constant(&buf, pattern, base, 0);
            assert!(failures.len() == 3);
            check!(failures[0].word_index == 0);
            check!(failures[1].word_index == 127);
            check!(failures[2].word_index == 255);
        }

        #[test]
        fn constant_empty_buffer() {
            let mut buf: Vec<u64> = vec![];
            fill_constant(&mut buf, 0xFF);
            let failures = verify_constant(&buf, 0xFF, 0, 0);
            assert!(failures.is_empty());
        }

        #[test]
        fn indexed_round_trip() {
            let mut buf = vec![0u64; 256];
            fill_indexed(&mut buf, 0);
            let base = buf.as_ptr() as usize;
            let failures = verify_indexed(&buf, base, 0);
            assert!(failures.is_empty());
        }

        #[test]
        fn indexed_round_trip_with_offset() {
            // start=100: buf[i] should equal 100+i
            let mut buf = vec![0u64; 64];
            fill_indexed(&mut buf, 100);
            for (i, &val) in buf.iter().enumerate() {
                check!(val == (100 + i) as u64, "mismatch at i={i}");
            }
            let base = buf.as_ptr() as usize;
            let failures = verify_indexed(&buf, base, 100);
            assert!(failures.is_empty());
        }

        #[test]
        fn indexed_detects_single_corruption() {
            let mut buf = vec![0u64; 256];
            fill_indexed(&mut buf, 0);
            buf[50] = 0xDEAD;
            let base = buf.as_ptr() as usize;
            let failures = verify_indexed(&buf, base, 0);
            assert!(failures.len() == 1);
            check!(failures[0].word_index == 50);
            check!(failures[0].expected == 50);
            check!(failures[0].actual == 0xDEAD);
            check!(failures[0].addr == base + 50 * 8);
        }

        #[test]
        fn indexed_detects_multiple_corruptions() {
            let mut buf = vec![0u64; 64];
            fill_indexed(&mut buf, 0);
            buf[0] = 999;
            buf[63] = 999;
            let base = buf.as_ptr() as usize;
            let failures = verify_indexed(&buf, base, 0);
            assert!(failures.len() == 2);
            check!(failures[0].word_index == 0);
            check!(failures[1].word_index == 63);
        }

        #[test]
        fn indexed_empty_buffer() {
            let mut buf: Vec<u64> = vec![];
            fill_indexed(&mut buf, 0);
            let failures = verify_indexed(&buf, 0, 0);
            assert!(failures.is_empty());
        }
    }

    mod orchestration {
        use assert2::assert;

        use super::*;

        static NOOP_ACTIVITY: fn(f64) = |_| {};

        #[test]
        fn constant_serial_clean() {
            let mut buf = vec![0u64; 1024];
            let failures =
                fill_verify_constant(&mut buf, 0xDEAD_BEEF_CAFE_BABE, false, &NOOP_ACTIVITY);
            assert!(failures.is_empty());
        }

        #[test]
        fn constant_parallel_clean() {
            let mut buf = vec![0u64; 4096];
            let failures =
                fill_verify_constant(&mut buf, 0x5555_5555_5555_5555, true, &NOOP_ACTIVITY);
            assert!(failures.is_empty());
        }

        #[test]
        fn indexed_serial_clean() {
            let mut buf = vec![0u64; 1024];
            let failures = fill_verify_indexed(&mut buf, false, &NOOP_ACTIVITY);
            assert!(failures.is_empty());
        }

        #[test]
        fn indexed_parallel_clean() {
            let mut buf = vec![0u64; 4096];
            let failures = fill_verify_indexed(&mut buf, true, &NOOP_ACTIVITY);
            assert!(failures.is_empty());
        }

        #[test]
        fn constant_serial_activity_fires() {
            let mut buf = vec![0u64; 1024];
            let count = std::sync::atomic::AtomicU32::new(0);
            let _ = fill_verify_constant(&mut buf, 0xFF, false, &|_| {
                count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            });
            assert!(count.load(std::sync::atomic::Ordering::Relaxed) > 0);
        }

        #[test]
        fn indexed_serial_activity_fires() {
            let mut buf = vec![0u64; 1024];
            let count = std::sync::atomic::AtomicU32::new(0);
            let _ = fill_verify_indexed(&mut buf, false, &|_| {
                count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            });
            assert!(count.load(std::sync::atomic::Ordering::Relaxed) > 0);
        }

        #[test]
        fn constant_empty() {
            let mut buf: Vec<u64> = vec![];
            let failures = fill_verify_constant(&mut buf, 0xFF, false, &NOOP_ACTIVITY);
            assert!(failures.is_empty());
        }

        #[test]
        fn indexed_empty() {
            let mut buf: Vec<u64> = vec![];
            let failures = fill_verify_indexed(&mut buf, false, &NOOP_ACTIVITY);
            assert!(failures.is_empty());
        }
    }
}
