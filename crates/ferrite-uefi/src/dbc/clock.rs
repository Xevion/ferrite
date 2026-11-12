//! Millisecond wall clock from the TSC, calibrated against the firmware's `stall`.

use core::arch::x86_64::_rdtsc;
use core::time::Duration;

use uefi::boot;

const CALIBRATION_MS: u64 = 10;

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
        boot::stall(Duration::from_millis(CALIBRATION_MS));
        let t1 = rdtsc();
        let per_ms = (t1.wrapping_sub(t0) / CALIBRATION_MS).max(1);
        Self { start: t1, per_ms }
    }

    #[must_use]
    pub fn ms(&self) -> u64 {
        rdtsc().wrapping_sub(self.start) / self.per_ms
    }
}

fn rdtsc() -> u64 {
    // SAFETY: RDTSC has no preconditions on x86_64.
    unsafe { _rdtsc() }
}
