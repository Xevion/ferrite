//! Clock, power and energy amounts.

use core::fmt;
use core::time::Duration;

/// A clock frequency.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Mhz(pub u32);

impl fmt::Display for Mhz {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} MHz", self.0)
    }
}

/// The average clock while a core ran, from the TSC frequency and how far `APERF` (counting at the
/// running clock) and `MPERF` (at the TSC's) advanced; `None` when MPERF did not move or the result
/// is zero.
#[must_use]
pub fn effective_mhz(tsc_hz: u64, aperf: u64, mperf: u64) -> Option<Mhz> {
    if mperf == 0 {
        return None;
    }
    let mhz = u128::from(tsc_hz) * u128::from(aperf) / u128::from(mperf) / 1_000_000;
    u32::try_from(mhz).ok().filter(|&mhz| mhz > 0).map(Mhz)
}

/// Power, in milliwatts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MilliWatts(pub u32);

impl fmt::Display for MilliWatts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:02} W", self.0 / 1000, self.0 % 1000 / 10)
    }
}

/// Energy, in microjoules.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MicroJoules(pub u64);

impl MicroJoules {
    /// The average power over `over`; `None` for an empty interval.
    #[must_use]
    pub fn per(self, over: Duration) -> Option<MilliWatts> {
        let micros = over.as_micros();
        if micros == 0 {
            return None;
        }
        // uJ / us = W, so uJ * 1000 / us = mW.
        u32::try_from(u128::from(self.0) * 1000 / micros).ok().map(MilliWatts)
    }

    #[must_use]
    pub const fn saturating_add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }
}

impl fmt::Display for MicroJoules {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:03} J", self.0 / 1_000_000, self.0 % 1_000_000 / 1000)
    }
}
