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

/// Write `pattern` to every word, then verify. Calls `on_complete` once finished.
fn fill_and_verify(
    buf_ptr: *const u8,
    buf: &mut [u64],
    pattern: u64,
    parallel: bool,
    on_complete: &mut impl FnMut(),
) -> Vec<Failure> {
    let failures = if parallel {
        // Write phase — each thread owns a disjoint chunk, no data races.
        buf.par_iter_mut().for_each(|word| {
            // SAFETY: word points into our mlock'd allocation.
            unsafe { ptr::write_volatile(word as *mut u64, pattern) };
        });

        // Verify phase — addr computed from buf base; equivalent to buf_ptr + i*8
        // since buf starts at the same address as buf_ptr.
        let base_addr = buf.as_ptr() as usize;
        buf.par_iter()
            .enumerate()
            .filter_map(|(i, word)| {
                // SAFETY: word points into our mlock'd allocation.
                let actual = unsafe { ptr::read_volatile(word as *const u64) };
                if actual != pattern {
                    Some(Failure {
                        addr: base_addr + i * 8,
                        expected: pattern,
                        actual,
                        word_index: i,
                    })
                } else {
                    None
                }
            })
            .collect()
    } else {
        let base = buf.as_ptr();
        for word in buf.iter_mut() {
            // SAFETY: word points into our mlock'd allocation.
            unsafe { ptr::write_volatile(word as *mut u64, pattern) };
        }
        let mut failures = Vec::new();
        for (i, word) in buf.iter().enumerate() {
            // SAFETY: word points into our mlock'd allocation.
            let actual = unsafe { ptr::read_volatile(word as *const u64) };
            if actual != pattern {
                failures.push(Failure {
                    addr: unsafe { (base.add(i) as *const u8).offset_from(buf_ptr) } as usize
                        + buf_ptr as usize,
                    expected: pattern,
                    actual,
                    word_index: i,
                });
            }
        }
        failures
    };

    on_complete();
    failures
}

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
    let failures = if parallel {
        buf.par_iter_mut().enumerate().for_each(|(i, word)| {
            // SAFETY: word points into our mlock'd allocation.
            unsafe { ptr::write_volatile(word as *mut u64, i as u64) };
        });

        let base_addr = buf.as_ptr() as usize;
        buf.par_iter()
            .enumerate()
            .filter_map(|(i, word)| {
                let expected = i as u64;
                // SAFETY: word points into our mlock'd allocation.
                let actual = unsafe { ptr::read_volatile(word as *const u64) };
                if actual != expected {
                    Some(Failure {
                        addr: base_addr + i * 8,
                        expected,
                        actual,
                        word_index: i,
                    })
                } else {
                    None
                }
            })
            .collect()
    } else {
        let base = buf.as_ptr();
        for (i, word) in buf.iter_mut().enumerate() {
            // SAFETY: word points into our mlock'd allocation.
            unsafe { ptr::write_volatile(word as *mut u64, i as u64) };
        }
        let mut failures = Vec::new();
        for (i, word) in buf.iter().enumerate() {
            let expected = i as u64;
            // SAFETY: word points into our mlock'd allocation.
            let actual = unsafe { ptr::read_volatile(word as *const u64) };
            if actual != expected {
                failures.push(Failure {
                    addr: unsafe { (base.add(i) as *const u8).offset_from(buf_ptr) } as usize
                        + buf_ptr as usize,
                    expected,
                    actual,
                    word_index: i,
                });
            }
        }
        failures
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
