use std::fmt;

pub mod alloc;
pub mod dimm;
pub mod edac;
pub mod error_analysis;
pub mod output;
pub mod pattern;
pub mod phys;
pub mod runner;
pub mod simd;
pub mod smbios;
pub mod stability;
pub mod units;

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
    pub phys_addr: Option<phys::PhysAddr>,
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
