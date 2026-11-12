//! Wall clock from the TSC, calibrated against the firmware's `stall`.
//!
//! This is the one place that reads the TSC and busy-waits through boot
//! services; everything else goes through the `ferrite-hal` traits.

use core::arch::x86_64::_rdtsc;
use core::time::Duration;

use ferrite_hal::{Delay, Instant};
use uefi::boot;

const CALIBRATION: Duration = Duration::from_millis(10);
const NANOS_PER_MS: u128 = 1_000_000;

#[derive(Clone, Copy, Debug)]
pub struct Clock {
    start: u64,
    per_ms: u64,
}

impl Clock {
    /// Calibrate (blocks for 10 ms) and start counting from now.
    #[must_use]
    pub fn start() -> Self {
        let t0 = rdtsc();
        stall(CALIBRATION);
        let t1 = rdtsc();
        let per_ms = (t1.wrapping_sub(t0) / CALIBRATION.as_millis() as u64).max(1);
        Self { start: t1, per_ms }
    }
}

/// Instants count from [`Clock::start`].
impl ferrite_hal::Clock for Clock {
    fn now(&self) -> Instant {
        let ticks = u128::from(rdtsc().wrapping_sub(self.start));
        let nanos = ticks * NANOS_PER_MS / u128::from(self.per_ms);
        Instant::from_epoch(Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX)))
    }
}

impl Delay for Clock {
    fn delay(&self, duration: Duration) {
        stall(duration);
    }
}

/// Raw TSC ticks, for seeding a generator when the firmware has no RNG; never a time source.
#[must_use]
pub fn tsc_entropy() -> u64 {
    rdtsc()
}

/// Busy-wait through boot services, for the rare caller with no [`Clock`] at hand.
#[expect(clippy::disallowed_methods, reason = "the clock's Delay is the one sanctioned busy-wait")]
pub(crate) fn stall(duration: Duration) {
    boot::stall(duration);
}

#[expect(clippy::disallowed_methods, reason = "the clock is the one reader of the TSC")]
fn rdtsc() -> u64 {
    // SAFETY: RDTSC has no preconditions on x86_64.
    unsafe { _rdtsc() }
}
