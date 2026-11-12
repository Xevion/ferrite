//! An incremental write/verify pattern sweep over one test buffer.
//!
//! Each pattern gets a write pass and then a verify pass over the whole buffer,
//! a chunk per [`Sweep::step`] so the caller can interleave other work. Every
//! access is a volatile `u64`, so the compiler can neither drop the stores as
//! dead nor satisfy the loads from what it just wrote.

use alloc::vec::Vec;
use core::fmt;
use core::ptr;

/// Patterns applied in order; each byte is repeated across the word.
const PATTERNS: [u8; 4] = [0x00, 0xFF, 0x55, 0xAA];
const WORD: usize = size_of::<u64>();

/// Smallest buffer worth sweeping when the requested size cannot be allocated.
pub const MIN_SWEEP_BYTES: usize = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Write,
    Verify,
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Write => "write",
            Self::Verify => "verify",
        })
    }
}

/// The CPU address of a word in the test buffer, physical under UEFI's identity map.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CpuAddr(usize);

impl CpuAddr {
    #[must_use]
    pub fn of<T>(ptr: *const T) -> Self {
        Self(ptr.addr())
    }

    #[must_use]
    pub const fn get(self) -> usize {
        self.0
    }
}

impl fmt::Display for CpuAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}

/// A word that read back differently from what was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fault {
    pub addr: CpuAddr,
    pub expected: u64,
    pub actual: u64,
}

/// Sweep progress for the UI, the status page and the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SweepStatus {
    pub bytes: usize,
    /// Progress through the current pass; wraps to 0 before reaching 100.
    pub percent: u16,
    pub pattern: u8,
    pub phase: Phase,
    pub passes: u64,
    /// Mismatched words over all passes.
    pub faults: u64,
    pub first_fault: Option<Fault>,
}

impl fmt::Display for SweepStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#04x} {} {}% pass={} faults={}", self.pattern, self.phase, self.percent, self.passes, self.faults)
    }
}

pub struct Sweep {
    buf: Vec<u64>,
    pattern_idx: usize,
    phase: Phase,
    /// Next word to process.
    offset: usize,
    passes: u64,
    faults: u64,
    first_fault: Option<Fault>,
}

impl Sweep {
    /// Allocate a test buffer of up to `bytes`, halving down to [`MIN_SWEEP_BYTES`]
    /// when the pool cannot supply it. With nothing left the sweep has no work.
    #[must_use]
    pub fn new(bytes: usize) -> Self {
        Self::with_buffer(alloc_largest(bytes, MIN_SWEEP_BYTES, try_alloc_words))
    }

    fn with_buffer(buf: Vec<u64>) -> Self {
        Self { buf, pattern_idx: 0, phase: Phase::Write, offset: 0, passes: 0, faults: 0, first_fault: None }
    }

    pub const fn has_work(&self) -> bool {
        !self.buf.is_empty()
    }

    /// Process up to `chunk_bytes` (at least one word) of the current phase,
    /// moving to the next phase, pattern or pass at the end of the buffer.
    pub fn step(&mut self, chunk_bytes: usize) {
        let len = self.buf.len();
        if len == 0 {
            return;
        }
        let start = self.offset;
        let end = start.saturating_add((chunk_bytes / WORD).max(1)).min(len);
        let pattern = u64::from_ne_bytes([PATTERNS[self.pattern_idx]; WORD]);
        let base = self.buf.as_mut_ptr();

        match self.phase {
            Phase::Write => {
                for i in start..end {
                    // SAFETY: `i < len`, and `base` is valid for `len` words.
                    unsafe { ptr::write_volatile(base.add(i), pattern) };
                }
            },
            Phase::Verify => {
                for i in start..end {
                    // SAFETY: as above.
                    let word = unsafe { base.add(i) };
                    // SAFETY: `word` is in bounds, as above.
                    let actual = unsafe { ptr::read_volatile(word) };
                    if actual != pattern {
                        self.record_fault(Fault { addr: CpuAddr::of(word), expected: pattern, actual });
                    }
                }
            },
        }

        self.offset = end;
        if end == len {
            self.offset = 0;
            self.advance_phase();
        }
    }

    fn record_fault(&mut self, fault: Fault) {
        self.faults = self.faults.saturating_add(1);
        self.first_fault.get_or_insert(fault);
    }

    fn advance_phase(&mut self) {
        match self.phase {
            Phase::Write => self.phase = Phase::Verify,
            Phase::Verify => {
                self.phase = Phase::Write;
                self.pattern_idx += 1;
                if self.pattern_idx == PATTERNS.len() {
                    self.pattern_idx = 0;
                    self.passes += 1;
                }
            },
        }
    }

    #[must_use]
    pub fn status(&self) -> SweepStatus {
        let len = self.buf.len() as u64;
        let phases_total = (PATTERNS.len() * 2) as u64;
        let phases_done = (self.pattern_idx * 2 + usize::from(self.phase == Phase::Verify)) as u64;
        let percent = if len == 0 { 0 } else { (phases_done * len + self.offset as u64) * 100 / (phases_total * len) };
        SweepStatus {
            bytes: self.buf.len() * WORD,
            percent: percent as u16,
            pattern: PATTERNS[self.pattern_idx],
            phase: self.phase,
            passes: self.passes,
            faults: self.faults,
            first_fault: self.first_fault,
        }
    }
}

fn try_alloc_words(bytes: usize) -> Option<Vec<u64>> {
    let words = bytes / WORD;
    let mut buf = Vec::new();
    buf.try_reserve_exact(words).ok()?;
    buf.resize(words, 0);
    Some(buf)
}

/// The largest buffer `try_alloc` gives, starting at `want` bytes and halving
/// down to `min`; empty when even `min` fails.
fn alloc_largest(want: usize, min: usize, mut try_alloc: impl FnMut(usize) -> Option<Vec<u64>>) -> Vec<u64> {
    let mut bytes = want;
    loop {
        if let Some(buf) = try_alloc(bytes) {
            if bytes < want {
                log::warn!("sweep: {want} byte buffer unavailable, testing {bytes} bytes");
            }
            return buf;
        }
        if bytes <= min {
            log::error!("sweep: no buffer of {min} bytes or more available, sweep disabled");
            return Vec::new();
        }
        bytes = (bytes / 2).max(min);
    }
}

#[cfg(test)]
mod tests;
