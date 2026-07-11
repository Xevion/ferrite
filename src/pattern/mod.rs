use std::fmt;

use crate::ops;
use crate::{Failure, FailureBudget};

mod checkerboard;
mod march;
pub mod metadata;
mod solid;
mod stuck_address;
mod walking;

use metadata::{
    Complexity, FaultClass, PatternMetadata,
    PatternTier::{Quick, Standard, Thorough},
};

/// All supported test patterns.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, strum::EnumCount, serde::Serialize,
)]
pub enum Pattern {
    SolidBits,
    WalkingOnes,
    WalkingZeros,
    Checkerboard,
    StuckAddress,
    MarchCMinus,
}

impl Pattern {
    pub const ALL: &[Self] = &[
        Self::SolidBits,
        Self::WalkingOnes,
        Self::WalkingZeros,
        Self::Checkerboard,
        Self::StuckAddress,
        Self::MarchCMinus,
    ];

    /// Number of fill-and-verify sub-passes this pattern performs.
    /// Used to size the inner progress bar.
    #[must_use]
    pub const fn sub_passes(&self) -> u64 {
        match self {
            Self::SolidBits | Self::Checkerboard => 2,
            Self::WalkingOnes | Self::WalkingZeros => 64,
            Self::StuckAddress => 1,
            // M0–M5 of the March C- sequence.
            Self::MarchCMinus => 6,
        }
    }

    /// Static metadata describing this pattern's fault coverage, cost, and tier
    /// membership.
    #[must_use]
    pub const fn metadata(&self) -> PatternMetadata {
        match self {
            Self::SolidBits => PatternMetadata {
                fault_classes: &[FaultClass::StuckAt],
                complexity: Complexity::Linear,
                requires_physical_order: false,
                is_destructive: false,
                tiers: &[Quick, Standard, Thorough],
            },
            // Walking bits stress data-line shorts (coupling) on top of stuck
            // bits by driving one hot bit against all its neighbors.
            Self::WalkingOnes | Self::WalkingZeros => PatternMetadata {
                fault_classes: &[FaultClass::StuckAt, FaultClass::Coupling],
                complexity: Complexity::LinearK(64),
                requires_physical_order: false,
                is_destructive: false,
                tiers: &[Standard, Thorough],
            },
            Self::Checkerboard => PatternMetadata {
                fault_classes: &[FaultClass::StuckAt, FaultClass::Coupling],
                complexity: Complexity::Linear,
                requires_physical_order: false,
                is_destructive: false,
                tiers: &[Quick, Standard, Thorough],
            },
            Self::StuckAddress => PatternMetadata {
                fault_classes: &[FaultClass::AddressDecoder],
                complexity: Complexity::Linear,
                requires_physical_order: false,
                is_destructive: false,
                tiers: &[Quick, Standard, Thorough],
            },
            Self::MarchCMinus => PatternMetadata {
                fault_classes: &[
                    FaultClass::StuckAt,
                    FaultClass::Transition,
                    FaultClass::AddressDecoder,
                    FaultClass::Coupling,
                ],
                complexity: Complexity::LinearK(10),
                requires_physical_order: false,
                is_destructive: false,
                tiers: &[Standard, Thorough],
            },
        }
    }
}

impl fmt::Display for Pattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SolidBits => write!(f, "Solid Bits"),
            Self::WalkingOnes => write!(f, "Walking Ones"),
            Self::WalkingZeros => write!(f, "Walking Zeros"),
            Self::Checkerboard => write!(f, "Checkerboard"),
            Self::StuckAddress => write!(f, "Stuck Address"),
            Self::MarchCMinus => write!(f, "March C-"),
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
/// `budget` caps how many failures this pattern collects; once exhausted the
/// pattern stops early (see [`FailureBudget`]).
pub fn run_pattern(
    pattern: Pattern,
    buf: &mut [u64],
    parallel: bool,
    budget: &FailureBudget,
    on_subpass: &mut impl FnMut(),
    on_activity: &(dyn Fn(f64) + Sync),
) -> Vec<Failure> {
    match pattern {
        Pattern::SolidBits => solid::run(buf, parallel, budget, on_subpass, on_activity),
        Pattern::WalkingOnes => walking::run_ones(buf, parallel, budget, on_subpass, on_activity),
        Pattern::WalkingZeros => walking::run_zeros(buf, parallel, budget, on_subpass, on_activity),
        Pattern::Checkerboard => checkerboard::run(buf, parallel, budget, on_subpass, on_activity),
        Pattern::StuckAddress => stuck_address::run(buf, parallel, budget, on_subpass, on_activity),
        Pattern::MarchCMinus => march::run(buf, parallel, budget, on_subpass, on_activity),
    }
}

/// Fill with `pattern`, verify (capping failures at `budget`), then call
/// `on_complete`.
pub(super) fn fill_and_verify(
    buf: &mut [u64],
    pattern: u64,
    parallel: bool,
    budget: &FailureBudget,
    on_complete: &mut impl FnMut(),
    on_activity: &(dyn Fn(f64) + Sync),
) -> Vec<Failure> {
    let failures = ops::fill_verify_constant(buf, pattern, parallel, budget, on_activity);
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

    /// Parametrized clean-memory tests -- one case per pattern x serial/parallel.
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
        #[case(Pattern::MarchCMinus)]
        fn serial(#[case] pattern: Pattern) {
            let mut buf = make_test_buf();
            let failures = run_pattern(
                pattern,
                &mut buf,
                false,
                &FailureBudget::unlimited(),
                &mut || {},
                &NOOP_ACTIVITY,
            );
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
        #[case(Pattern::MarchCMinus)]
        fn parallel(#[case] pattern: Pattern) {
            let mut buf = make_test_buf();
            let failures = run_pattern(
                pattern,
                &mut buf,
                true,
                &FailureBudget::unlimited(),
                &mut || {},
                &NOOP_ACTIVITY,
            );
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
        solid::run(
            &mut buf,
            false,
            &FailureBudget::unlimited(),
            &mut || count += 1,
            &NOOP_ACTIVITY,
        );
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
            "Pattern::ALL is missing variants -- update it when adding new patterns"
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
        #[case(Pattern::MarchCMinus, "March C-", 6)]
        fn display_and_sub_passes(
            #[case] pattern: Pattern,
            #[case] expected_name: &str,
            #[case] expected_sub_passes: u64,
        ) {
            check!(pattern.to_string() == expected_name);
            check!(pattern.sub_passes() == expected_sub_passes);
        }

        use metadata::{Complexity, FaultClass, PatternTier};

        /// Every pattern must declare at least one fault class and belong to at
        /// least one tier -- a pattern that detects nothing or runs in no tier
        /// is dead weight and almost certainly a metadata mistake.
        #[test]
        fn every_pattern_has_faults_and_tiers() {
            for &pattern in Pattern::ALL {
                let meta = pattern.metadata();
                check!(
                    !meta.fault_classes.is_empty(),
                    "pattern {pattern} declares no fault classes"
                );
                check!(
                    !meta.tiers.is_empty(),
                    "pattern {pattern} belongs to no tier"
                );
            }
        }

        /// The Thorough tier is the superset -- anything a lighter tier runs,
        /// Thorough must also run, or "thorough" is a lie.
        #[test]
        fn thorough_tier_is_a_superset() {
            for &pattern in Pattern::ALL {
                let tiers = pattern.metadata().tiers;
                if tiers.contains(&PatternTier::Quick) || tiers.contains(&PatternTier::Standard) {
                    check!(
                        tiers.contains(&PatternTier::Thorough),
                        "pattern {pattern} runs in a lighter tier but not Thorough"
                    );
                }
            }
        }

        #[rstest]
        #[case(Pattern::SolidBits, &[FaultClass::StuckAt], Complexity::Linear)]
        #[case(
            Pattern::WalkingOnes,
            &[FaultClass::StuckAt, FaultClass::Coupling],
            Complexity::LinearK(64)
        )]
        #[case(
            Pattern::WalkingZeros,
            &[FaultClass::StuckAt, FaultClass::Coupling],
            Complexity::LinearK(64)
        )]
        #[case(
            Pattern::Checkerboard,
            &[FaultClass::StuckAt, FaultClass::Coupling],
            Complexity::Linear
        )]
        #[case(
            Pattern::StuckAddress,
            &[FaultClass::AddressDecoder],
            Complexity::Linear
        )]
        #[case(
            Pattern::MarchCMinus,
            &[
                FaultClass::StuckAt,
                FaultClass::Transition,
                FaultClass::AddressDecoder,
                FaultClass::Coupling,
            ],
            Complexity::LinearK(10)
        )]
        fn fault_classes_and_complexity(
            #[case] pattern: Pattern,
            #[case] expected_faults: &[FaultClass],
            #[case] expected_complexity: Complexity,
        ) {
            let meta = pattern.metadata();
            check!(meta.fault_classes == expected_faults);
            check!(meta.complexity == expected_complexity);
            check!(!meta.requires_physical_order);
            check!(!meta.is_destructive);
        }
    }

    mod cancellation {
        use assert2::check;
        use serial_test::serial;

        use super::*;
        use crate::shutdown::{self, QuitReason};

        /// A quit requested mid-pattern must stop the sub-pass loop early
        /// instead of grinding through all 64 walking-ones cycles.
        #[test]
        #[serial]
        fn walking_ones_stops_when_quit_requested_mid_pattern() {
            shutdown::reset();
            let mut buf = make_test_buf();
            let mut sub_passes = 0u32;
            run_pattern(
                Pattern::WalkingOnes,
                &mut buf,
                false,
                &FailureBudget::unlimited(),
                &mut || {
                    sub_passes += 1;
                    if sub_passes == 3 {
                        shutdown::request_quit(QuitReason::UserQuit);
                    }
                },
                &NOOP_ACTIVITY,
            );
            shutdown::reset();
            check!(sub_passes == 3);
        }

        /// Without a quit, the pattern runs every sub-pass to completion.
        #[test]
        #[serial]
        fn walking_ones_runs_all_sub_passes_without_quit() {
            shutdown::reset();
            let mut buf = make_test_buf();
            let mut sub_passes = 0u32;
            run_pattern(
                Pattern::WalkingOnes,
                &mut buf,
                false,
                &FailureBudget::unlimited(),
                &mut || sub_passes += 1,
                &NOOP_ACTIVITY,
            );
            check!(sub_passes == 64);
        }
    }

    mod dispatch {
        use assert2::assert;
        use std::ptr;

        use super::*;

        #[test]
        fn fill_verify_constant_clean_memory() {
            let mut buf = make_test_buf();
            let failures = ops::fill_verify_constant(
                &mut buf,
                0xFFFF_FFFF_FFFF_FFFF,
                false,
                &FailureBudget::unlimited(),
                &NOOP_ACTIVITY,
            );
            assert!(failures.is_empty());
        }

        #[test]
        fn fill_verify_constant_single_word() {
            let mut buf = vec![0u64; 1];
            let failures = ops::fill_verify_constant(
                &mut buf,
                0xDEAD_BEEF,
                false,
                &FailureBudget::unlimited(),
                &NOOP_ACTIVITY,
            );
            assert!(failures.is_empty());
        }

        #[test]
        fn fill_verify_indexed_single_word() {
            let mut buf = vec![0u64; 1];
            let failures = ops::fill_verify_indexed(
                &mut buf,
                false,
                &FailureBudget::unlimited(),
                &NOOP_ACTIVITY,
            );
            assert!(failures.is_empty());
        }

        #[test]
        fn fill_verify_constant_parallel_clean() {
            let mut buf = vec![0u64; 4096];
            let failures = ops::fill_verify_constant(
                &mut buf,
                0x5555_5555_5555_5555,
                true,
                &FailureBudget::unlimited(),
                &NOOP_ACTIVITY,
            );
            assert!(failures.is_empty());
        }

        #[test]
        fn fill_verify_indexed_parallel_clean() {
            let mut buf = vec![0u64; 4096];
            let failures = ops::fill_verify_indexed(
                &mut buf,
                true,
                &FailureBudget::unlimited(),
                &NOOP_ACTIVITY,
            );
            assert!(failures.is_empty());
        }

        #[test]
        fn fill_verify_constant_empty_buffer() {
            let mut buf: Vec<u64> = vec![];
            let failures = ops::fill_verify_constant(
                &mut buf,
                0xFF,
                false,
                &FailureBudget::unlimited(),
                &NOOP_ACTIVITY,
            );
            assert!(failures.is_empty());
        }

        #[test]
        fn fill_verify_indexed_empty_buffer() {
            let mut buf: Vec<u64> = vec![];
            let failures = ops::fill_verify_indexed(
                &mut buf,
                false,
                &FailureBudget::unlimited(),
                &NOOP_ACTIVITY,
            );
            assert!(failures.is_empty());
        }

        #[test]
        fn fill_and_verify_calls_on_complete() {
            let mut buf = vec![0u64; 64];
            let mut called = false;
            let _ = fill_and_verify(
                &mut buf,
                0xAA,
                false,
                &FailureBudget::unlimited(),
                &mut || called = true,
                &NOOP_ACTIVITY,
            );
            assert!(called);
        }

        #[test]
        fn non_chunk_multiple_buffer_size() {
            // 1000 is not a multiple of REPORT_CHUNK (64*1024), testing partial chunk handling
            let mut buf = vec![0u64; 1000];
            let failures = ops::fill_verify_constant(
                &mut buf,
                0x1234_5678_9ABC_DEF0,
                false,
                &FailureBudget::unlimited(),
                &NOOP_ACTIVITY,
            );
            assert!(failures.is_empty());

            let failures = ops::fill_verify_indexed(
                &mut buf,
                false,
                &FailureBudget::unlimited(),
                &NOOP_ACTIVITY,
            );
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
