//! What the headless log on the boot volume records while the app runs.
//!
//! A target box with no screen gives no other feedback, so link and lease
//! changes, the first memory fault and a periodic alive line go to the
//! [`BootLog`] behind the `log` sink. A missing log file means the app never
//! started: a firmware boot-policy problem, not a code one.

use core::time::Duration;

use ferrite_efi::bootlog::BootLog;
use ferrite_efi::logger;

use crate::net::{Link, NetStatus};
use crate::sweep::SweepStatus;

/// Name of the log file, `\EFI\ferrite\ferrite-uefi.txt`.
pub const LOG_NAME: &str = "ferrite-uefi";
/// A healthy but quiet box still proves its loop is turning this often.
const ALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// Log `msg` and save the file now: for boot, net up, faults and exit.
pub fn milestone(msg: &str) {
    logger::with(|log| log.milestone(msg));
}

#[derive(Default)]
pub struct Diag {
    last_link: Option<Link>,
    fault_logged: bool,
    last_alive: Duration,
}

impl Diag {
    #[must_use]
    pub const fn new() -> Self {
        Self { last_link: None, fault_logged: false, last_alive: Duration::ZERO }
    }

    /// Log what changed since the last call, and let the log save itself when due.
    pub fn tick(&mut self, now: Duration, net: Option<&NetStatus>, sweep: &SweepStatus) {
        if let Some(net) = net {
            self.link_changed(&net.link);
        }
        if !self.fault_logged
            && let Some(fault) = sweep.first_fault
        {
            self.fault_logged = true;
            milestone(&alloc::format!(
                "sweep: FAULT at {}: expected {:#018x}, read {:#018x} (pattern {:#04x}, pass {})",
                fault.addr,
                fault.expected,
                fault.actual,
                sweep.pattern,
                sweep.passes
            ));
        }
        if now.saturating_sub(self.last_alive) >= ALIVE_INTERVAL {
            self.last_alive = now;
            match net {
                Some(net) => log::debug!("alive sweep: {sweep} net: {}", net.counters),
                None => log::debug!("alive sweep: {sweep}"),
            }
        }
        logger::with(BootLog::poll);
    }

    fn link_changed(&mut self, link: &Link) {
        let previous = self.last_link.replace(*link);
        if previous == Some(*link) {
            return;
        }
        let was_up = previous.is_some_and(|p| p.up);
        let had_address = previous.and_then(|p| p.address);
        if link.address.is_some() && link.address != had_address {
            milestone(&alloc::format!("net: up, {link}"));
        } else if was_up && !link.up {
            log::warn!("net: link lost, {link}");
        } else if had_address.is_some() && link.address.is_none() {
            log::warn!("net: lease lost, {link}");
        } else {
            log::debug!("net: {link}");
        }
    }
}
