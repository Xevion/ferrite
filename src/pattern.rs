use std::fmt;
use std::ptr;

use rayon::prelude::*;

use crate::Failure;
#[cfg(target_arch = "x86_64")]
use crate::simd::{
    CHUNK, avx512_available, fill_nt, fill_nt_indexed, verify_avx512, verify_indexed_avx512,
};

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
/// All reads and writes use volatile operations to prevent the compiler from
/// optimizing away the memory accesses.
///
/// `parallel` enables multi-threaded write and verify phases via Rayon.
/// `on_subpass` is called after each internal fill-and-verify sub-pass, suitable
/// for driving a progress bar in the caller.
pub fn run_pattern(
    pattern: Pattern,
    buf: &mut [u64],
    parallel: bool,
    on_subpass: &mut impl FnMut(),
) -> Vec<Failure> {
    match pattern {
        Pattern::SolidBits => test_solid_bits(buf, parallel, on_subpass),
        Pattern::WalkingOnes => test_walking_ones(buf, parallel, on_subpass),
        Pattern::WalkingZeros => test_walking_zeros(buf, parallel, on_subpass),
        Pattern::Checkerboard => test_checkerboard(buf, parallel, on_subpass),
        Pattern::StuckAddress => test_stuck_address(buf, parallel, on_subpass),
    }
}

/// Fill every word with `pattern`, then verify. Returns any mismatches.
fn fill_verify_constant(buf: &mut [u64], pattern: u64, parallel: bool) -> Vec<Failure> {
    let base_addr = buf.as_ptr() as usize;

    #[cfg(target_arch = "x86_64")]
    if avx512_available() {
        return if parallel {
            buf.par_chunks_mut(CHUNK).for_each(|chunk| {
                // SAFETY: chunk starts at a 64-byte aligned address (mmap base is
                // page-aligned; every CHUNK * 8 byte boundary is 64-byte aligned).
                unsafe { fill_nt(chunk, pattern) };
            });
            // Rayon's join barrier ensures all NT stores and sfences have completed.
            buf.par_chunks(CHUNK)
                .enumerate()
                .flat_map_iter(|(ci, chunk)| {
                    // SAFETY: same alignment argument as write side.
                    unsafe { verify_avx512(chunk, pattern, base_addr, ci * CHUNK) }
                })
                .collect()
        } else {
            unsafe {
                fill_nt(buf, pattern);
                verify_avx512(buf, pattern, base_addr, 0)
            }
        };
    }

    if parallel {
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
                    phys_addr: None,
                })
            })
            .collect()
    } else {
        for word in buf.iter_mut() {
            unsafe { ptr::write_volatile(word as *mut u64, pattern) };
        }
        buf.iter()
            .enumerate()
            .filter_map(|(i, word)| {
                let actual = unsafe { ptr::read_volatile(word as *const u64) };
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
}

/// Fill every word with its index, then verify. Returns any mismatches.
fn fill_verify_indexed(buf: &mut [u64], parallel: bool) -> Vec<Failure> {
    let base_addr = buf.as_ptr() as usize;

    #[cfg(target_arch = "x86_64")]
    if avx512_available() {
        return if parallel {
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
        } else {
            unsafe {
                fill_nt_indexed(buf, 0);
                verify_indexed_avx512(buf, base_addr, 0)
            }
        };
    }

    if parallel {
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
                    phys_addr: None,
                })
            })
            .collect()
    } else {
        for (i, word) in buf.iter_mut().enumerate() {
            unsafe { ptr::write_volatile(word as *mut u64, i as u64) };
        }
        buf.iter()
            .enumerate()
            .filter_map(|(i, word)| {
                let expected = i as u64;
                let actual = unsafe { ptr::read_volatile(word as *const u64) };
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
}

/// Fill with `pattern`, verify, then call `on_complete`.
fn fill_and_verify(
    buf: &mut [u64],
    pattern: u64,
    parallel: bool,
    on_complete: &mut impl FnMut(),
) -> Vec<Failure> {
    let failures = fill_verify_constant(buf, pattern, parallel);
    on_complete();
    failures
}

fn test_solid_bits(buf: &mut [u64], parallel: bool, on_subpass: &mut impl FnMut()) -> Vec<Failure> {
    let mut failures = Vec::new();
    failures.extend(fill_and_verify(
        buf,
        0x0000_0000_0000_0000,
        parallel,
        on_subpass,
    ));
    failures.extend(fill_and_verify(
        buf,
        0xFFFF_FFFF_FFFF_FFFF,
        parallel,
        on_subpass,
    ));
    failures
}

fn test_walking_ones(
    buf: &mut [u64],
    parallel: bool,
    on_subpass: &mut impl FnMut(),
) -> Vec<Failure> {
    let mut failures = Vec::new();
    for bit in 0..64 {
        let pattern = 1u64 << bit;
        failures.extend(fill_and_verify(buf, pattern, parallel, on_subpass));
    }
    failures
}

fn test_walking_zeros(
    buf: &mut [u64],
    parallel: bool,
    on_subpass: &mut impl FnMut(),
) -> Vec<Failure> {
    let mut failures = Vec::new();
    for bit in 0..64 {
        let pattern = !(1u64 << bit);
        failures.extend(fill_and_verify(buf, pattern, parallel, on_subpass));
    }
    failures
}

fn test_checkerboard(
    buf: &mut [u64],
    parallel: bool,
    on_subpass: &mut impl FnMut(),
) -> Vec<Failure> {
    let mut failures = Vec::new();
    failures.extend(fill_and_verify(
        buf,
        0xAAAA_AAAA_AAAA_AAAA,
        parallel,
        on_subpass,
    ));
    failures.extend(fill_and_verify(
        buf,
        0x5555_5555_5555_5555,
        parallel,
        on_subpass,
    ));
    failures
}

fn test_stuck_address(
    buf: &mut [u64],
    parallel: bool,
    on_subpass: &mut impl FnMut(),
) -> Vec<Failure> {
    let failures = fill_verify_indexed(buf, parallel);
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
        let failures = test_solid_bits(&mut buf, false, &mut || {});
        assert!(failures.is_empty());
    }

    #[test]
    fn walking_ones_no_failures() {
        let mut buf = make_test_buf();
        let failures = test_walking_ones(&mut buf, false, &mut || {});
        assert!(failures.is_empty());
    }

    #[test]
    fn checkerboard_no_failures() {
        let mut buf = make_test_buf();
        let failures = test_checkerboard(&mut buf, false, &mut || {});
        assert!(failures.is_empty());
    }

    #[test]
    fn stuck_address_no_failures() {
        let mut buf = make_test_buf();
        let failures = test_stuck_address(&mut buf, false, &mut || {});
        assert!(failures.is_empty());
    }

    #[test]
    fn parallel_solid_bits_no_failures() {
        let mut buf = make_test_buf();
        let failures = test_solid_bits(&mut buf, true, &mut || {});
        assert!(failures.is_empty());
    }

    #[test]
    fn parallel_walking_ones_no_failures() {
        let mut buf = make_test_buf();
        let failures = test_walking_ones(&mut buf, true, &mut || {});
        assert!(failures.is_empty());
    }

    #[test]
    fn parallel_stuck_address_no_failures() {
        let mut buf = make_test_buf();
        let failures = test_stuck_address(&mut buf, true, &mut || {});
        assert!(failures.is_empty());
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
        assert_eq!(f.flipped_bits(), 1);
        let s = f.to_string();
        assert!(s.contains("FAIL"));
        assert!(s.contains("1 bit(s)"));
    }
}
