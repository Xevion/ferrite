use std::fmt;

use crate::phys::PhysAddr;

/// A single test failure record.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Failure {
    /// Virtual address of the failing word.
    pub addr: usize,
    /// Expected value.
    pub expected: u64,
    /// Actual value read back.
    pub actual: u64,
    /// Word index within the buffer.
    pub word_index: usize,
    /// Physical address, if resolved via pagemap.
    pub phys_addr: Option<PhysAddr>,
}

impl Failure {
    #[must_use]
    pub fn xor(&self) -> u64 {
        self.expected ^ self.actual
    }

    #[must_use]
    pub fn flipped_bits(&self) -> u32 {
        self.xor().count_ones()
    }

    /// Bit positions (0-63) that differ between expected and actual.
    #[must_use]
    pub fn bit_positions(&self) -> Vec<u8> {
        let xor = self.xor();
        (0u8..64).filter(|&bit| xor & (1u64 << bit) != 0).collect()
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FAIL  addr=0x{:016x}", self.addr)?;
        if let Some(phys) = self.phys_addr {
            write!(f, "  phys={phys}")?;
        }
        write!(
            f,
            "  expected=0x{:016x}  actual=0x{:016x}  xor=0x{:016x} ({} bit(s))",
            self.expected,
            self.actual,
            self.xor(),
            self.flipped_bits(),
        )
    }
}

/// Builder for constructing [`Failure`] values in tests.
///
/// `word_index` defaults to `addr / 8` when not set explicitly.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct FailureBuilder {
    addr: usize,
    expected: u64,
    actual: u64,
    word_index: Option<usize>,
    phys_addr: Option<PhysAddr>,
}

#[cfg(test)]
impl FailureBuilder {
    pub(crate) fn addr(mut self, addr: usize) -> Self {
        self.addr = addr;
        self
    }

    pub(crate) fn expected(mut self, expected: u64) -> Self {
        self.expected = expected;
        self
    }

    pub(crate) fn actual(mut self, actual: u64) -> Self {
        self.actual = actual;
        self
    }

    pub(crate) fn phys(mut self, phys: u64) -> Self {
        self.phys_addr = Some(PhysAddr(phys));
        self
    }

    pub(crate) fn build(self) -> Failure {
        Failure {
            addr: self.addr,
            expected: self.expected,
            actual: self.actual,
            word_index: self.word_index.unwrap_or(self.addr / 8),
            phys_addr: self.phys_addr,
        }
    }
}

#[cfg(test)]
mod tests {
    use assert2::assert;
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn failure_display_with_phys_addr() {
        let f = FailureBuilder::default()
            .addr(0x1000)
            .expected(0xFF)
            .actual(0xFE)
            .phys(0xdead_beef)
            .build();
        let s = f.to_string();
        assert!(s.contains("phys=0xdeadbeef"));
        assert!(s.contains("1 bit(s)"));
    }

    #[test]
    fn failure_display_without_phys_addr() {
        let f = FailureBuilder::default()
            .addr(0x2000)
            .expected(0xFF)
            .actual(0xFE)
            .build();
        let s = f.to_string();
        assert!(!s.contains("phys="));
        assert!(s.contains("1 bit(s)"));
    }

    #[test]
    fn bit_positions_single_bit() {
        let f = FailureBuilder::default()
            .expected(0x00)
            .actual(0x08) // bit 3
            .build();
        assert!(f.bit_positions() == vec![3]);
    }

    #[test]
    fn bit_positions_multiple_bits() {
        let f = FailureBuilder::default()
            .expected(0x00)
            .actual(0b1010_0101) // bits 0, 2, 5, 7
            .build();
        assert!(f.bit_positions() == vec![0, 2, 5, 7]);
    }

    #[test]
    fn bit_positions_no_diff() {
        let f = FailureBuilder::default()
            .expected(0xAA)
            .actual(0xAA)
            .build();
        assert!(f.bit_positions().is_empty());
    }

    #[test]
    fn bit_positions_high_bit() {
        let f = FailureBuilder::default()
            .expected(0)
            .actual(1u64 << 63)
            .build();
        assert!(f.bit_positions() == vec![63]);
    }

    #[test]
    fn builder_word_index_default() {
        let f = FailureBuilder::default().addr(0x100).build();
        assert!(f.word_index == 0x100 / 8);
    }

    #[test]
    fn xor_and_flipped_bits() {
        let f = FailureBuilder::default()
            .expected(0xFF00_FF00_FF00_FF00)
            .actual(0x00FF_00FF_00FF_00FF)
            .build();
        assert!(f.xor() == 0xFFFF_FFFF_FFFF_FFFF);
        assert!(f.flipped_bits() == 64);
    }

    proptest! {
        #[test]
        fn flipped_bits_at_most_64(expected: u64, actual: u64) {
            let f = FailureBuilder::default()
                .expected(expected)
                .actual(actual)
                .build();
            prop_assert!(f.flipped_bits() <= 64);
        }

        #[test]
        fn bit_positions_count_matches_flipped_bits(expected: u64, actual: u64) {
            let f = FailureBuilder::default()
                .expected(expected)
                .actual(actual)
                .build();
            prop_assert!(f.bit_positions().len() as u32 == f.flipped_bits());
        }
    }
}
