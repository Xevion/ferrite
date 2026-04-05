use std::fmt;

use crate::Failure;
use crate::ops;

mod checkerboard;
mod solid;
mod stuck_address;
mod walking;

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

/// Fill with `pattern`, verify, then call `on_complete`.
pub(super) fn fill_and_verify(
    buf: &mut [u64],
    pattern: u64,
    parallel: bool,
    on_complete: &mut impl FnMut(),
    on_activity: &(dyn Fn(f64) + Sync),
) -> Vec<Failure> {
    let failures = ops::fill_verify_constant(buf, pattern, parallel, on_activity);
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

    mod pattern_metadata {
        use assert2::check;
        use rstest::rstest;

        use super::*;

        #[rstest]
        #[case(Pattern::SolidBits, "Solid Bits", 2)]
        #[case(Pattern::WalkingOnes, "Walking Ones", 64)]
        #[case(Pattern::WalkingZeros, "Walking Zeros", 64)]
        #[case(Pattern::Checkerboard, "Checkerboard", 2)]
        #[case(Pattern::StuckAddress, "Stuck Address", 1)]
        fn display_and_sub_passes(
            #[case] pattern: Pattern,
            #[case] expected_name: &str,
            #[case] expected_sub_passes: u64,
        ) {
            check!(pattern.to_string() == expected_name);
            check!(pattern.sub_passes() == expected_sub_passes);
        }
    }

    mod dispatch {
        use assert2::assert;
        use std::ptr;

        use super::*;

        #[test]
        fn fill_verify_constant_clean_memory() {
            let mut buf = make_test_buf();
            let failures =
                ops::fill_verify_constant(&mut buf, 0xFFFF_FFFF_FFFF_FFFF, false, &NOOP_ACTIVITY);
            assert!(failures.is_empty());
        }

        #[test]
        fn fill_verify_constant_single_word() {
            let mut buf = vec![0u64; 1];
            let failures = ops::fill_verify_constant(&mut buf, 0xDEAD_BEEF, false, &NOOP_ACTIVITY);
            assert!(failures.is_empty());
        }

        #[test]
        fn fill_verify_indexed_single_word() {
            let mut buf = vec![0u64; 1];
            let failures = ops::fill_verify_indexed(&mut buf, false, &NOOP_ACTIVITY);
            assert!(failures.is_empty());
        }

        #[test]
        fn fill_verify_constant_parallel_clean() {
            let mut buf = vec![0u64; 4096];
            let failures =
                ops::fill_verify_constant(&mut buf, 0x5555_5555_5555_5555, true, &NOOP_ACTIVITY);
            assert!(failures.is_empty());
        }

        #[test]
        fn fill_verify_indexed_parallel_clean() {
            let mut buf = vec![0u64; 4096];
            let failures = ops::fill_verify_indexed(&mut buf, true, &NOOP_ACTIVITY);
            assert!(failures.is_empty());
        }

        #[test]
        fn fill_verify_constant_empty_buffer() {
            let mut buf: Vec<u64> = vec![];
            let failures = ops::fill_verify_constant(&mut buf, 0xFF, false, &NOOP_ACTIVITY);
            assert!(failures.is_empty());
        }

        #[test]
        fn fill_verify_indexed_empty_buffer() {
            let mut buf: Vec<u64> = vec![];
            let failures = ops::fill_verify_indexed(&mut buf, false, &NOOP_ACTIVITY);
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
                ops::fill_verify_constant(&mut buf, 0x1234_5678_9ABC_DEF0, false, &NOOP_ACTIVITY);
            assert!(failures.is_empty());

            let failures = ops::fill_verify_indexed(&mut buf, false, &NOOP_ACTIVITY);
            assert!(failures.is_empty());
        }

        #[test]
        fn constant_detects_single_corruption_serial() {
            let mut buf = vec![0u64; 1024];
            let pattern = 0xAAAA_AAAA_AAAA_AAAAu64;
            for word in &mut buf {
                unsafe { ptr::write_volatile(std::ptr::from_mut::<u64>(word), pattern) };
            }
            buf[42] = 0xBBBB_BBBB_BBBB_BBBBu64;
            let base = buf.as_ptr() as usize;
            let failures = ops::scalar::verify_constant(&buf, pattern, base, 0);
            assert!(failures.len() == 1);
        }
    }
}
