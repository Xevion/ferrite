//! Which bits fail, and whether they fail the same way every time.
//!
//! A cell stuck at one value flips the same bits in the same direction in every fault it causes;
//! coupling and disturbance flip different bits from one fault to the next.

use core::fmt;

use crate::fault::{CpuAddr, Fault};

/// Bit-flip statistics over every fault recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FaultStats {
    faults: u64,
    lowest: Option<CpuAddr>,
    highest: Option<CpuAddr>,
    flipped: u64,
    flips_per_bit: [u32; 64],
    /// AND over all faults of the bits that read 1 where 0 was written.
    stuck_high: Option<u64>,
    /// AND over all faults of the bits that read 0 where 1 was written.
    stuck_low: Option<u64>,
}

impl Default for FaultStats {
    fn default() -> Self {
        Self::new()
    }
}

impl FaultStats {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            faults: 0,
            lowest: None,
            highest: None,
            flipped: 0,
            flips_per_bit: [0; 64],
            stuck_high: None,
            stuck_low: None,
        }
    }

    pub fn record(&mut self, fault: &Fault) {
        self.faults = self.faults.saturating_add(1);
        self.lowest = Some(self.lowest.map_or(fault.addr, |a| a.min(fault.addr)));
        self.highest = Some(self.highest.map_or(fault.addr, |a| a.max(fault.addr)));
        let xor = fault.expected ^ fault.actual;
        self.flipped |= xor;
        for (bit, count) in self.flips_per_bit.iter_mut().enumerate() {
            *count = count.saturating_add(u32::from(xor >> bit & 1 != 0));
        }
        let high = fault.actual & !fault.expected;
        let low = fault.expected & !fault.actual;
        self.stuck_high = Some(self.stuck_high.map_or(high, |m| m & high));
        self.stuck_low = Some(self.stuck_low.map_or(low, |m| m & low));
    }

    #[must_use]
    pub const fn faults(&self) -> u64 {
        self.faults
    }

    /// The lowest and highest faulty addresses.
    #[must_use]
    pub fn span(&self) -> Option<(CpuAddr, CpuAddr)> {
        self.lowest.zip(self.highest)
    }

    /// Every bit that flipped at least once.
    #[must_use]
    pub const fn flipped(&self) -> u64 {
        self.flipped
    }

    #[must_use]
    pub const fn flips_per_bit(&self) -> &[u32; 64] {
        &self.flips_per_bit
    }

    /// Bits that read 1 where 0 was written in every fault.
    #[must_use]
    pub fn stuck_high(&self) -> u64 {
        self.stuck_high.unwrap_or(0)
    }

    /// Bits that read 0 where 1 was written in every fault.
    #[must_use]
    pub fn stuck_low(&self) -> u64 {
        self.stuck_low.unwrap_or(0)
    }

    #[must_use]
    pub fn classification(&self) -> Classification {
        if self.faults == 0 {
            return Classification::Clean;
        }
        let stuck = self.stuck_high() | self.stuck_low();
        if stuck == 0 {
            Classification::Coupling
        } else if stuck == self.flipped {
            Classification::StuckBits { mask: stuck }
        } else {
            Classification::Mixed { stuck }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Classification {
    Clean,
    /// The same bits flip the same way in every fault: a hard stuck bit.
    StuckBits {
        mask: u64,
    },
    /// No bit flips in every fault: coupling or disturbance.
    Coupling,
    /// Some bits are stuck, others flip from fault to fault.
    Mixed {
        stuck: u64,
    },
}

impl fmt::Display for Classification {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Clean => f.write_str("no faults"),
            Self::StuckBits { mask } => write!(f, "stuck bits {mask:#x}"),
            Self::Coupling => f.write_str("coupling (bits vary)"),
            Self::Mixed { stuck } => write!(f, "mixed (stuck {stuck:#x}, others vary)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;
    use rstest::rstest;

    use super::*;

    fn fault(addr: usize, expected: u64, actual: u64) -> Fault {
        Fault { addr: CpuAddr::of(core::ptr::without_provenance::<u64>(addr)), expected, actual }
    }

    #[rstest]
    #[case::stuck_high(&[(0x1000, 0, 1 << 20), (0x3000, 0x0f, 0x0f | 1 << 20)], Classification::StuckBits { mask: 1 << 20 })]
    #[case::stuck_low(&[(0x1000, 1 << 5, 0), (0x2000, !0, !(1 << 5))], Classification::StuckBits { mask: 1 << 5 })]
    #[case::bits_vary(&[(0x1000, 0xff, 0xfe), (0x2000, 0xff, 0xfd)], Classification::Coupling)]
    #[case::one_stuck_one_varies(&[(0x1000, 0, 1 << 20 | 1 << 5), (0x2000, 0, 1 << 20)], Classification::Mixed { stuck: 1 << 20 })]
    // A bit stuck high only shows where 0 was written, so a 1 -> 0 fault elsewhere breaks the AND.
    #[case::opposite_directions(&[(0x1000, 0, 1), (0x2000, 1, 0)], Classification::Coupling)]
    fn classifies_by_which_bits_flip_every_time(#[case] faults: &[(usize, u64, u64)], #[case] want: Classification) {
        let mut stats = FaultStats::new();
        for &(addr, expected, actual) in faults {
            stats.record(&fault(addr, expected, actual));
        }
        check!(stats.classification() == want);
    }

    #[test]
    fn tracks_the_faulty_span_and_per_bit_counts() {
        let mut stats = FaultStats::new();
        for (addr, flip) in [(0x2000, 0b101), (0x1000, 0b001), (0x3000, 0b100)] {
            stats.record(&fault(addr, 0, flip));
        }
        check!(stats.span() == Some((fault(0x1000, 0, 0).addr, fault(0x3000, 0, 0).addr)));
        check!(stats.flips_per_bit()[..3] == [2, 0, 2]);
        check!(stats.flipped() == 0b101);
    }
}
