//! An incremental memory sweep, the first sliver of ferrite's actual job.
//!
//! It allocates a test buffer and walks it through a handful of patterns, doing
//! a write pass then a verify pass for each. Work is done in small chunks per
//! [`step`](Sweep::step) so the render loop and network stack stay responsive;
//! the UI reads [`status`](Sweep::status) each frame to drive the gauge.
//!
//! All buffer access goes through `read_volatile`/`write_volatile`: a RAM test
//! is precisely the case where the compiler would otherwise prove the stores
//! dead and delete them.

use alloc::vec;
use alloc::vec::Vec;
use core::ptr;

/// Patterns applied in order; classic stuck-bit / all-ones / alternating checks.
const PATTERNS: [u8; 4] = [0x00, 0xFF, 0x55, 0xAA];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Write,
    Verify,
}

/// A single detected mismatch.
#[derive(Clone, Copy)]
pub struct Fault {
    pub offset: usize,
    pub expected: u8,
    pub actual: u8,
}

/// A snapshot of sweep progress for the UI / HTTP page.
pub struct SweepStatus {
    pub bytes: usize,
    pub percent: u16,
    pub pattern: u8,
    pub verifying: bool,
    pub passes: u64,
    pub faults: u64,
    pub first_fault: Option<Fault>,
}

pub struct Sweep {
    buf: Vec<u8>,
    pattern_idx: usize,
    phase: Phase,
    offset: usize,
    passes: u64,
    faults: u64,
    first_fault: Option<Fault>,
}

impl Sweep {
    /// Allocate a `bytes`-sized test buffer.
    pub fn new(bytes: usize) -> Self {
        Self {
            buf: vec![0u8; bytes],
            pattern_idx: 0,
            phase: Phase::Write,
            offset: 0,
            passes: 0,
            faults: 0,
            first_fault: None,
        }
    }

    /// Process up to `chunk` bytes of the current phase, advancing phase/pattern
    /// (and looping back for another pass) at buffer boundaries.
    pub fn step(&mut self, chunk: usize) {
        let len = self.buf.len();
        if len == 0 {
            return;
        }
        let end = (self.offset + chunk).min(len);
        let pattern = PATTERNS[self.pattern_idx];
        let base = self.buf.as_mut_ptr();

        match self.phase {
            Phase::Write => {
                for i in self.offset..end {
                    // SAFETY: `i < len` and `base` is valid for `len` bytes.
                    // Volatile so the store cannot be elided as dead.
                    unsafe { ptr::write_volatile(base.add(i), pattern) };
                }
            }
            Phase::Verify => {
                for i in self.offset..end {
                    // SAFETY: as above; volatile read forces an actual load from
                    // memory rather than reusing the value we just wrote.
                    let got = unsafe { ptr::read_volatile(base.add(i)) };
                    if got != pattern {
                        self.faults += 1;
                        if self.first_fault.is_none() {
                            self.first_fault = Some(Fault {
                                offset: i,
                                expected: pattern,
                                actual: got,
                            });
                        }
                    }
                }
            }
        }

        self.offset = end;
        if self.offset >= len {
            self.offset = 0;
            match self.phase {
                Phase::Write => self.phase = Phase::Verify,
                Phase::Verify => {
                    self.phase = Phase::Write;
                    self.pattern_idx += 1;
                    if self.pattern_idx >= PATTERNS.len() {
                        self.pattern_idx = 0;
                        self.passes += 1;
                    }
                }
            }
        }
    }

    #[must_use]
    pub fn status(&self) -> SweepStatus {
        let len = self.buf.len();
        let phases_total = (PATTERNS.len() * 2) as u64;
        let phases_done = (self.pattern_idx * 2 + usize::from(self.phase == Phase::Verify)) as u64;
        let percent = if len == 0 {
            0
        } else {
            let num = phases_done * len as u64 + self.offset as u64;
            let den = phases_total * len as u64;
            ((num * 100) / den) as u16
        };
        SweepStatus {
            bytes: len,
            percent,
            pattern: PATTERNS[self.pattern_idx],
            verifying: self.phase == Phase::Verify,
            passes: self.passes,
            faults: self.faults,
            first_fault: self.first_fault,
        }
    }
}
