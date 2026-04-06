use crate::Failure;

/// Aggregate bit-flip statistics across multiple failures.
#[derive(Debug, Clone)]
pub struct BitErrorStats {
    pub total_failures: usize,
    /// Lowest physical address with an error (None if no physical addresses available).
    pub lowest_phys: Option<u64>,
    /// Highest physical address with an error.
    pub highest_phys: Option<u64>,
    /// OR of all XOR masks -- which bit positions have ever flipped.
    pub union_xor_mask: u64,
    /// Count of flips per bit position (index 0 = bit 0).
    pub bit_positions: [u32; 64],
    /// Bits that always flipped from 0->1 across all errors.
    /// Computed as: AND of (actual & ~expected) across all errors.
    stuck_high_accum: Option<u64>,
    /// Bits that always flipped from 1->0 across all errors.
    /// Computed as: AND of (expected & ~actual) across all errors.
    stuck_low_accum: Option<u64>,
}

impl Default for BitErrorStats {
    fn default() -> Self {
        Self::new()
    }
}

impl BitErrorStats {
    #[must_use]
    pub fn new() -> Self {
        Self {
            total_failures: 0,
            lowest_phys: None,
            highest_phys: None,
            union_xor_mask: 0,
            bit_positions: [0; 64],
            stuck_high_accum: None,
            stuck_low_accum: None,
        }
    }

    /// Accumulate a failure into the statistics.
    pub fn record(&mut self, failure: &Failure) {
        self.total_failures += 1;

        if let Some(phys) = failure.phys_addr {
            let p = phys.0;
            self.lowest_phys = Some(self.lowest_phys.map_or(p, |prev| prev.min(p)));
            self.highest_phys = Some(self.highest_phys.map_or(p, |prev| prev.max(p)));
        }

        let xor = failure.xor();
        self.union_xor_mask |= xor;

        for bit in 0..64 {
            if xor & (1u64 << bit) != 0 {
                self.bit_positions[bit] += 1;
            }
        }

        // Stuck-high: bits that went 0->1 (set in actual, clear in expected)
        let high = failure.actual & !failure.expected;
        // Stuck-low: bits that went 1->0 (set in expected, clear in actual)
        let low = failure.expected & !failure.actual;

        self.stuck_high_accum = Some(match self.stuck_high_accum {
            Some(prev) => prev & high,
            None => high,
        });
        self.stuck_low_accum = Some(match self.stuck_low_accum {
            Some(prev) => prev & low,
            None => low,
        });
    }

    /// Bits that flipped 0->1 in every single error (consistent stuck-high).
    #[must_use]
    pub fn stuck_high_mask(&self) -> u64 {
        self.stuck_high_accum.unwrap_or(0)
    }

    /// Bits that flipped 1->0 in every single error (consistent stuck-low).
    #[must_use]
    pub fn stuck_low_mask(&self) -> u64 {
        self.stuck_low_accum.unwrap_or(0)
    }

    /// Classify the overall error pattern.
    #[must_use]
    pub fn classification(&self) -> ErrorClassification {
        if self.total_failures == 0 {
            return ErrorClassification::NoErrors;
        }

        let stuck_mask = self.stuck_high_mask() | self.stuck_low_mask();
        if stuck_mask == 0 {
            return ErrorClassification::Coupling;
        }

        // If the stuck mask accounts for all flipped bits, it's pure stuck-bit
        if stuck_mask == self.union_xor_mask {
            let positions: Vec<u8> = (0..64)
                .filter(|&bit| stuck_mask & (1u64 << bit) != 0)
                .collect();
            return ErrorClassification::StuckBit { positions };
        }

        // Some bits are consistently stuck, others vary
        ErrorClassification::Mixed
    }
}

/// Classification of error patterns across all failures.
#[derive(Debug, PartialEq, Eq)]
pub enum ErrorClassification {
    /// No errors recorded.
    NoErrors,
    /// Same bit position(s) flip in every error -- likely a hard stuck bit.
    StuckBit { positions: Vec<u8> },
    /// Different bits flip across errors -- coupling or disturbance faults.
    Coupling,
    /// Some bits are consistently stuck, but other bits also flip inconsistently.
    Mixed,
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};
    use proptest::prelude::*;

    use super::*;
    use crate::failure::FailureBuilder;

    fn arb_failure() -> impl Strategy<Value = crate::Failure> {
        (any::<usize>(), any::<u64>(), any::<u64>()).prop_map(|(addr, expected, actual)| {
            FailureBuilder::default()
                .addr(addr)
                .expected(expected)
                .actual(actual)
                .build()
        })
    }

    fn f(addr: usize, expected: u64, actual: u64) -> crate::Failure {
        FailureBuilder::default()
            .addr(addr)
            .expected(expected)
            .actual(actual)
            .build()
    }

    fn f_phys(addr: usize, expected: u64, actual: u64, phys: u64) -> crate::Failure {
        FailureBuilder::default()
            .addr(addr)
            .expected(expected)
            .actual(actual)
            .phys(phys)
            .build()
    }

    #[test]
    fn default_is_new() {
        let d = BitErrorStats::default();
        let n = BitErrorStats::new();
        check!(d.total_failures == n.total_failures);
        check!(d.union_xor_mask == n.union_xor_mask);
    }

    #[test]
    fn empty_stats() {
        let stats = BitErrorStats::new();
        check!(stats.total_failures == 0);
        assert!(stats.classification() == ErrorClassification::NoErrors);
    }

    #[test]
    fn bit_position_counts() {
        let mut stats = BitErrorStats::new();
        // Bits 0, 1, and 2 flip -- bit 0 flips twice
        stats.record(&f(0x1000, 0x7, 0x4)); // bits 0,1 flipped
        stats.record(&f(0x2000, 0x5, 0x0)); // bits 0,2 flipped

        check!(stats.bit_positions[0] == 2);
        check!(stats.bit_positions[1] == 1);
        check!(stats.bit_positions[2] == 1);
        check!(stats.bit_positions[3] == 0);
        assert!(stats.union_xor_mask == 0x7);
    }

    mod classification {
        use assert2::{assert, check};

        use super::*;

        #[test]
        fn single_stuck_high_bit() {
            let mut stats = BitErrorStats::new();
            // Bit 20 flipped from 0 to 1 in every error, with physical addresses
            stats.record(&f_phys(0x1000, 0x0, 1 << 20, 0x2000));
            stats.record(&f_phys(0x2000, 0x0, 1 << 20, 0x3000));

            check!(stats.total_failures == 2);
            check!(stats.stuck_high_mask() == 1 << 20);
            check!(stats.stuck_low_mask() == 0);
            check!(stats.union_xor_mask == 1 << 20);
            check!(stats.bit_positions[20] == 2);
            check!(stats.lowest_phys == Some(0x2000));
            check!(stats.highest_phys == Some(0x3000));

            assert!(let ErrorClassification::StuckBit { positions } = stats.classification());
            assert!(positions == vec![20u8]);
        }

        #[test]
        fn single_stuck_low_bit() {
            let mut stats = BitErrorStats::new();
            // Bit 5 flipped from 1 to 0 in every error
            stats.record(&f(0x1000, 1 << 5, 0x0));
            stats.record(&f(0x2000, 1 << 5, 0x0));

            check!(stats.stuck_high_mask() == 0);
            check!(stats.stuck_low_mask() == 1 << 5);

            assert!(let ErrorClassification::StuckBit { positions } = stats.classification());
            assert!(positions == vec![5u8]);
        }

        #[test]
        fn coupling_errors_vary() {
            let mut stats = BitErrorStats::new();
            // Different bits flip each time -- no consistently stuck bit
            stats.record(&f(0x1000, 0xFF, 0xFE)); // bit 0 flipped
            stats.record(&f(0x2000, 0xFF, 0xFD)); // bit 1 flipped

            check!(stats.total_failures == 2);
            assert!(stats.classification() == ErrorClassification::Coupling);
        }

        #[test]
        fn mixed_errors() {
            let mut stats = BitErrorStats::new();
            // Bit 20 always flips 0->1, but bit 5 only flips once
            stats.record(&f(0x1000, 0x0, (1 << 20) | (1 << 5)));
            stats.record(&f(0x2000, 0x0, 1 << 20));

            check!(stats.stuck_high_mask() == 1 << 20);
            assert!(stats.classification() == ErrorClassification::Mixed);
        }
    }

    fn arb_stuck_high_failures() -> impl Strategy<Value = (u64, Vec<crate::Failure>)> {
        (1u64..=u64::MAX, any::<u64>(), 1usize..=12).prop_map(|(stuck_mask, base, count)| {
            let expected = base & !stuck_mask;
            let actual = expected | stuck_mask;
            let failures = (0..count)
                .map(|i| {
                    FailureBuilder::default()
                        .addr(i * 8)
                        .expected(expected)
                        .actual(actual)
                        .build()
                })
                .collect();
            (stuck_mask, failures)
        })
    }

    proptest! {
        #[test]
        fn classification_not_no_errors_after_record(
            failures in prop::collection::vec(arb_failure(), 1..=20)
        ) {
            let mut stats = BitErrorStats::new();
            for f in &failures {
                stats.record(f);
            }
            prop_assert!(!matches!(stats.classification(), ErrorClassification::NoErrors));
        }

        #[test]
        fn stuck_mask_subset_of_union_xor(
            failures in prop::collection::vec(arb_failure(), 1..=20)
        ) {
            let mut stats = BitErrorStats::new();
            for f in &failures {
                stats.record(f);
            }
            let stuck = stats.stuck_high_mask() | stats.stuck_low_mask();
            prop_assert_eq!(stuck & !stats.union_xor_mask, 0u64);
        }

        #[test]
        fn record_order_independence(
            keyed in prop::collection::vec((arb_failure(), any::<u8>()), 1..=12)
        ) {
            let mut permuted = keyed.clone();
            permuted.sort_by_key(|(_, k)| *k);

            let mut fwd = BitErrorStats::new();
            let mut perm = BitErrorStats::new();
            for (f, _) in &keyed { fwd.record(f); }
            for (f, _) in &permuted { perm.record(f); }

            prop_assert_eq!(fwd.total_failures, perm.total_failures);
            prop_assert_eq!(fwd.union_xor_mask, perm.union_xor_mask);
            prop_assert_eq!(fwd.bit_positions, perm.bit_positions);
            prop_assert_eq!(fwd.stuck_high_mask(), perm.stuck_high_mask());
            prop_assert_eq!(fwd.stuck_low_mask(), perm.stuck_low_mask());
        }

        #[test]
        fn stuckbit_positions_in_union_xor(
            failures in prop::collection::vec(arb_failure(), 1..=20)
        ) {
            let mut stats = BitErrorStats::new();
            for f in &failures {
                stats.record(f);
            }
            if let ErrorClassification::StuckBit { positions } = stats.classification() {
                for &pos in &positions {
                    prop_assert!(stats.union_xor_mask & (1u64 << pos) != 0);
                }
            }
        }

        #[test]
        fn stuckbit_positions_exhaustive(
            (stuck_mask, failures) in arb_stuck_high_failures()
        ) {
            let mut stats = BitErrorStats::new();
            for f in &failures {
                stats.record(f);
            }
            let expected_positions: Vec<u8> =
                (0u8..64).filter(|&b| stuck_mask & (1u64 << b) != 0).collect();
            match stats.classification() {
                ErrorClassification::StuckBit { positions } => {
                    prop_assert_eq!(positions, expected_positions);
                }
                other => prop_assert!(false, "expected StuckBit, got {other:?}"),
            }
        }
    }
}
