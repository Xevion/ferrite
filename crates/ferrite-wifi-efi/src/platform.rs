//! The scheduler's platform under UEFI: the TSC clock and no interrupts.

use ferrite_efi::clock::Clock;
use ferrite_hal::{Clock as _, Instant};
use ferrite_lkpi::sched::Platform;

/// Time from the calibrated TSC. The main loop polls and waits out deadlines itself, and firmware
/// notifies only set flags, so there is no wakeup to arm and no interrupt to report.
pub struct UefiPlatform {
    clock: Clock,
    epoch: Instant,
}

impl UefiPlatform {
    /// Platform time starts at zero here, as the host platform's does.
    pub fn new(clock: Clock) -> Self {
        Self { epoch: clock.now(), clock }
    }
}

impl Platform for UefiPlatform {
    fn now(&self) -> Instant {
        Instant::from_epoch(self.clock.now() - self.epoch)
    }

    fn arm_wakeup(&mut self, _at: Option<Instant>) {}

    fn take_pending_irqs(&mut self) -> bool {
        false
    }
}
