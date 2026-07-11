//! The [`Failure`] record: one mismatched word, the fundamental unit of a
//! memory fault in ferrite.
//!
//! Carries the virtual address, expected/actual values, and (when physical
//! resolution is available) the [`crate::physmem::phys::PhysAddr`] -- enough
//! to report and later classify ([`crate::error_analysis`]) every fault a
//! pattern finds.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::physmem::phys::PhysAddr;

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
    pub const fn xor(&self) -> u64 {
        self.expected ^ self.actual
    }

    #[must_use]
    pub const fn flipped_bits(&self) -> u32 {
        self.xor().count_ones()
    }

    /// Bit positions (0-63) that differ between expected and actual.
    #[must_use]
    pub fn flipped_bit_indices(&self) -> Vec<u8> {
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

/// A shared, thread-safe cap on how many [`Failure`] records a single pattern
/// run may collect.
///
/// On catastrophically-bad memory a pattern would otherwise materialize one
/// record per failing word -- hundreds of millions of them for a multi-GiB
/// buffer -- and exhaust RAM inside the verify collect before the runner ever
/// sees the result. The verify paths claim against this budget as they go,
/// truncating their contribution once the cap is reached, so total live
/// `Failure` memory stays bounded regardless of how bad the DIMM is.
///
/// A budget is created per pattern (the cap is per pattern) and shared across
/// that pattern's parallel chunks and sequential sub-passes.
pub struct FailureBudget {
    /// Slots still available. `usize::MAX` means unlimited.
    remaining: AtomicUsize,
    /// Set once the cap dropped at least one failure.
    overflowed: AtomicBool,
}

impl FailureBudget {
    /// Cap collection at `max` records. `max == 0` means unlimited.
    #[must_use]
    pub const fn new(max: usize) -> Self {
        let remaining = if max == 0 { usize::MAX } else { max };
        Self {
            remaining: AtomicUsize::new(remaining),
            overflowed: AtomicBool::new(false),
        }
    }

    /// An unlimited budget that never caps -- for benchmarks and tests.
    #[must_use]
    pub const fn unlimited() -> Self {
        Self::new(0)
    }

    /// True once the cap has been hit and at least one failure was dropped.
    #[must_use]
    pub fn overflowed(&self) -> bool {
        self.overflowed.load(Ordering::Relaxed)
    }

    /// True when no slots remain, so callers can skip further verify work.
    #[must_use]
    pub fn is_exhausted(&self) -> bool {
        self.remaining.load(Ordering::Relaxed) == 0
    }

    /// Truncate an independent batch of `failures` to the slots still available,
    /// marking the budget overflowed if any were dropped. Used by the parallel
    /// verify paths, where each chunk presents its own freshly-built batch.
    pub fn cap(&self, failures: &mut Vec<Failure>) {
        let want = failures.len();
        if want == 0 {
            return;
        }
        let granted = self.claim(want);
        if granted < want {
            failures.truncate(granted);
            self.mark_overflow();
        }
    }

    /// Claim up to `want` slots, returning how many were granted (0 when
    /// exhausted). Sequential accumulators (the march executor) claim only the
    /// growth since their last checkpoint.
    pub(crate) fn claim(&self, want: usize) -> usize {
        let mut cur = self.remaining.load(Ordering::Relaxed);
        loop {
            let grant = want.min(cur);
            if grant == 0 {
                return 0;
            }
            match self.remaining.compare_exchange_weak(
                cur,
                cur - grant,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return grant,
                Err(actual) => cur = actual,
            }
        }
    }

    /// Record that the cap dropped at least one failure.
    pub(crate) fn mark_overflow(&self) {
        self.overflowed.store(true, Ordering::Relaxed);
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
    fn flipped_bit_indices_single_bit() {
        let f = FailureBuilder::default()
            .expected(0x00)
            .actual(0x08) // bit 3
            .build();
        assert!(f.flipped_bit_indices() == vec![3]);
    }

    #[test]
    fn flipped_bit_indices_multiple_bits() {
        let f = FailureBuilder::default()
            .expected(0x00)
            .actual(0b1010_0101) // bits 0, 2, 5, 7
            .build();
        assert!(f.flipped_bit_indices() == vec![0, 2, 5, 7]);
    }

    #[test]
    fn flipped_bit_indices_no_diff() {
        let f = FailureBuilder::default()
            .expected(0xAA)
            .actual(0xAA)
            .build();
        assert!(f.flipped_bit_indices().is_empty());
    }

    #[test]
    fn flipped_bit_indices_high_bit() {
        let f = FailureBuilder::default()
            .expected(0)
            .actual(1u64 << 63)
            .build();
        assert!(f.flipped_bit_indices() == vec![63]);
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

    mod budget {
        use assert2::check;

        use super::super::{Failure, FailureBudget};

        fn failures(n: usize) -> Vec<Failure> {
            (0..n)
                .map(|i| Failure {
                    addr: i * 8,
                    expected: 0,
                    actual: 1,
                    word_index: i,
                    phys_addr: None,
                })
                .collect()
        }

        #[test]
        fn unlimited_never_caps() {
            let b = FailureBudget::unlimited();
            let mut f = failures(10_000);
            b.cap(&mut f);
            check!(f.len() == 10_000);
            check!(!b.overflowed());
            check!(!b.is_exhausted());
        }

        #[test]
        fn cap_truncates_single_batch_to_limit() {
            let b = FailureBudget::new(100);
            let mut f = failures(250);
            b.cap(&mut f);
            check!(f.len() == 100);
            check!(b.overflowed());
            check!(b.is_exhausted());
        }

        #[test]
        fn cap_spans_multiple_batches() {
            let b = FailureBudget::new(100);
            let mut a = failures(60);
            let mut c = failures(60);
            b.cap(&mut a);
            b.cap(&mut c);
            // First batch takes 60, second is truncated to the remaining 40.
            check!(a.len() == 60);
            check!(c.len() == 40);
            check!(b.overflowed());
            check!(b.is_exhausted());
        }

        #[test]
        fn cap_under_limit_leaves_batch_and_flag_untouched() {
            let b = FailureBudget::new(100);
            let mut f = failures(40);
            b.cap(&mut f);
            check!(f.len() == 40);
            check!(!b.overflowed());
            check!(!b.is_exhausted());
        }

        #[test]
        fn claim_grants_then_exhausts() {
            let b = FailureBudget::new(10);
            check!(b.claim(4) == 4);
            check!(b.claim(4) == 4);
            // Only 2 slots remain despite asking for 5.
            check!(b.claim(5) == 2);
            check!(b.claim(1) == 0);
            check!(b.is_exhausted());
        }

        #[test]
        fn empty_batch_is_a_noop() {
            let b = FailureBudget::new(10);
            let mut f: Vec<Failure> = Vec::new();
            b.cap(&mut f);
            check!(!b.overflowed());
            check!(!b.is_exhausted());
        }
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
        fn flipped_bit_indices_count_matches_flipped_bits(expected: u64, actual: u64) {
            let f = FailureBuilder::default()
                .expected(expected)
                .actual(actual)
                .build();
            prop_assert!(f.flipped_bit_indices().len() as u32 == f.flipped_bits());
        }
    }
}
