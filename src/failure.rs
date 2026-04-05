use std::fmt;

use crate::phys::PhysAddr;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_display_with_phys_addr() {
        let f = Failure {
            addr: 0x1000,
            expected: 0xFF,
            actual: 0xFE,
            word_index: 0,
            phys_addr: Some(PhysAddr(0xdead_beef)),
        };
        let s = f.to_string();
        assert!(s.contains("phys=0xdeadbeef"));
        assert!(s.contains("1 bit(s)"));
    }
}
