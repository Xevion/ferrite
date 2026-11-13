//! The scheduler's platform under UEFI: the TSC clock and polled interrupts.

use core::time::Duration;

use ferrite_efi::clock::Clock;
use ferrite_hal::{Clock as _, Instant};
use ferrite_lkpi::pci::PollCadence;
use ferrite_lkpi::sched::Platform;

/// How often the IRQ task runs the devices' handlers. Boot services offer no interrupt line, so every
/// poll step at least this long after the last one reports interrupts pending.
pub const IRQ_POLL: Duration = Duration::from_millis(1);

/// Time from the calibrated TSC, and interrupts pending on the poll cadence once a device exists.
///
/// The main loop polls and waits out deadlines itself, and firmware notifies only set flags, so there
/// is no wakeup to arm.
pub struct UefiPlatform {
    clock: Clock,
    epoch: Instant,
    irqs: PollCadence,
}

impl UefiPlatform {
    /// Platform time starts at zero here, as the host platform's does.
    pub fn new(clock: Clock) -> Self {
        Self { epoch: clock.now(), clock, irqs: PollCadence::new(IRQ_POLL) }
    }
}

impl Platform for UefiPlatform {
    fn now(&self) -> Instant {
        Instant::from_epoch(self.clock.now() - self.epoch)
    }

    fn arm_wakeup(&mut self, _at: Option<Instant>) {}

    fn take_pending_irqs(&mut self) -> bool {
        ferrite_lkpi::pci::device_count() > 0 && self.irqs.due(self.now())
    }
}
