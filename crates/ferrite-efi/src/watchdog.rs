//! The firmware watchdog: the one place that arms or disarms it.
//!
//! A cold reset on expiry is what turns a hang into a return to the default OS, so a long-running
//! app keeps re-arming it from its own loop with a [`Keepalive`] and a hang stops the re-arming.

use core::time::Duration;

use ferrite_hal::Instant;
use uefi::{Status, boot};

/// Identifies our arming to the firmware; codes above 0xffff are ours.
const CODE: u64 = 0x4645_5252;

/// Reset the machine if nothing re-arms or disarms the watchdog within `timeout`.
///
/// The firmware counts whole seconds, so `timeout` is rounded up, and a zero `timeout` is refused
/// because the firmware reads 0 as off.
#[expect(clippy::disallowed_methods, reason = "the watchdog module is the one home of arming the firmware watchdog")]
pub fn arm(timeout: Duration) -> uefi::Result {
    let secs = timeout.as_secs() + u64::from(timeout.subsec_nanos() > 0);
    if secs == 0 {
        return Err(Status::INVALID_PARAMETER.into());
    }
    let secs = usize::try_from(secs).map_err(|_| uefi::Error::from(Status::INVALID_PARAMETER))?;
    boot::set_watchdog_timer(secs, CODE, None)
}

/// Turn the watchdog off, between runs and at startup, where the firmware may have left its own armed.
#[expect(clippy::disallowed_methods, reason = "the watchdog module is the one home of disarming the firmware watchdog")]
pub fn disarm() {
    if let Err(e) = boot::set_watchdog_timer(0, CODE, None) {
        log::warn!("disarming the watchdog failed: {e}");
    }
}

/// Re-arms the watchdog for `timeout` whenever `every` has passed since the last time.
#[derive(Clone, Copy, Debug)]
pub struct Keepalive {
    timeout: Duration,
    every: Duration,
    next: Instant,
}

impl Keepalive {
    /// Due at once, so the first [`Keepalive::tick`] arms the watchdog.
    #[must_use]
    pub const fn new(timeout: Duration, every: Duration, now: Instant) -> Self {
        Self { timeout, every, next: now }
    }

    /// Arm now and schedule the next arming. A failure is logged and retried on the next tick.
    pub fn arm_now(&mut self, now: Instant) {
        self.next = now;
        self.tick(now);
    }

    /// Re-arm if the interval has passed.
    pub fn tick(&mut self, now: Instant) {
        if now < self.next {
            return;
        }
        match arm(self.timeout) {
            Ok(()) => self.next = now + self.every,
            Err(e) => log::warn!("arming the {} s watchdog failed: {e}", self.timeout.as_secs()),
        }
    }
}
