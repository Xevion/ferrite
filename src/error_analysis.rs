use crate::Failure;

/// Aggregate bit-flip statistics across multiple failures.
#[derive(Debug, Clone)]
pub struct BitErrorStats {
    pub total_errors: usize,
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
    pub fn new() -> Self {
        Self {
            total_errors: 0,
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
        self.total_errors += 1;

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
    pub fn stuck_high_mask(&self) -> u64 {
        self.stuck_high_accum.unwrap_or(0)
    }

    /// Bits that flipped 1->0 in every single error (consistent stuck-low).
    pub fn stuck_low_mask(&self) -> u64 {
        self.stuck_low_accum.unwrap_or(0)
    }

    /// Classify the overall error pattern.
    pub fn classification(&self) -> ErrorClassification {
        if self.total_errors == 0 {
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
    use super::*;
    use crate::phys::PhysAddr;

    fn make_failure(addr: usize, expected: u64, actual: u64, phys: Option<u64>) -> Failure {
        Failure {
            addr,
            expected,
            actual,
            word_index: addr / 8,
            phys_addr: phys.map(PhysAddr),
        }
    }

    #[test]
    fn empty_stats() {
        let stats = BitErrorStats::new();
        assert_eq!(stats.total_errors, 0);
        assert_eq!(stats.classification(), ErrorClassification::NoErrors);
    }

    #[test]
    fn single_stuck_high_bit() {
        let mut stats = BitErrorStats::new();
        // Bit 20 flipped from 0 to 1
        stats.record(&make_failure(0x1000, 0x0, 1 << 20, Some(0x2000)));
        stats.record(&make_failure(0x2000, 0x0, 1 << 20, Some(0x3000)));

        assert_eq!(stats.total_errors, 2);
        assert_eq!(stats.stuck_high_mask(), 1 << 20);
        assert_eq!(stats.stuck_low_mask(), 0);
        assert_eq!(stats.union_xor_mask, 1 << 20);
        assert_eq!(stats.bit_positions[20], 2);
        assert_eq!(stats.lowest_phys, Some(0x2000));
        assert_eq!(stats.highest_phys, Some(0x3000));
        assert_eq!(
            stats.classification(),
            ErrorClassification::StuckBit {
                positions: vec![20]
            }
        );
    }

    #[test]
    fn single_stuck_low_bit() {
        let mut stats = BitErrorStats::new();
        // Bit 5 flipped from 1 to 0
        stats.record(&make_failure(0x1000, 1 << 5, 0x0, None));
        stats.record(&make_failure(0x2000, 1 << 5, 0x0, None));

        assert_eq!(stats.stuck_high_mask(), 0);
        assert_eq!(stats.stuck_low_mask(), 1 << 5);
        assert_eq!(
            stats.classification(),
            ErrorClassification::StuckBit { positions: vec![5] }
        );
    }

    #[test]
    fn coupling_errors_vary() {
        let mut stats = BitErrorStats::new();
        // Different bits flip each time
        stats.record(&make_failure(0x1000, 0xFF, 0xFE, None)); // bit 0 flipped
        stats.record(&make_failure(0x2000, 0xFF, 0xFD, None)); // bit 1 flipped

        assert_eq!(stats.total_errors, 2);
        // No single bit is consistently stuck
        assert_eq!(stats.classification(), ErrorClassification::Coupling);
    }

    #[test]
    fn mixed_errors() {
        let mut stats = BitErrorStats::new();
        // Bit 20 always flips 0->1, but bit 5 only sometimes
        stats.record(&make_failure(0x1000, 0x0, (1 << 20) | (1 << 5), None));
        stats.record(&make_failure(0x2000, 0x0, 1 << 20, None));

        assert_eq!(stats.stuck_high_mask(), 1 << 20);
        // Bit 5 only flipped once, so it's not in the stuck mask
        assert_eq!(stats.classification(), ErrorClassification::Mixed);
    }

    #[test]
    fn bit_position_counts() {
        let mut stats = BitErrorStats::new();
        // Bits 0, 1, and 2 flip -- bit 0 flips twice
        stats.record(&make_failure(0x1000, 0x7, 0x4, None)); // bits 0,1 flipped
        stats.record(&make_failure(0x2000, 0x5, 0x0, None)); // bits 0,2 flipped

        assert_eq!(stats.bit_positions[0], 2);
        assert_eq!(stats.bit_positions[1], 1);
        assert_eq!(stats.bit_positions[2], 1);
        assert_eq!(stats.bit_positions[3], 0);
        assert_eq!(stats.union_xor_mask, 0x7);
    }
}
