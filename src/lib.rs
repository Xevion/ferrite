use std::fmt;

pub mod alloc;
pub mod pattern;
pub mod runner;
pub mod simd;

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
