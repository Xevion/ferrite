//! Addresses and mismatch reports.

use core::fmt;

/// The CPU address of a word under test, physical under UEFI's identity map.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
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

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: expected {:#018x}, read {:#018x}", self.addr, self.expected, self.actual)
    }
}

/// Where verifies report mismatches: every one is counted, the first `budget` reach the sink.
pub struct Faults<'a> {
    sink: &'a mut dyn FnMut(Fault),
    budget: u64,
    seen: u64,
}

impl<'a> Faults<'a> {
    pub fn new(budget: u64, sink: &'a mut dyn FnMut(Fault)) -> Self {
        Self { sink, budget, seen: 0 }
    }

    /// Mismatched words seen, including those past the budget.
    #[must_use]
    pub const fn seen(&self) -> u64 {
        self.seen
    }

    /// Mismatches handed to the sink.
    #[must_use]
    pub fn reported(&self) -> u64 {
        self.seen.min(self.budget)
    }

    #[must_use]
    pub const fn exhausted(&self) -> bool {
        self.seen >= self.budget
    }

    pub(crate) fn record(&mut self, fault: Fault) {
        if self.seen < self.budget {
            (self.sink)(fault);
        }
        self.seen = self.seen.saturating_add(1);
    }
}

impl fmt::Debug for Faults<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Faults").field("budget", &self.budget).field("seen", &self.seen).finish_non_exhaustive()
    }
}
