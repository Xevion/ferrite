//! What the headless log on the boot volume records while the app runs.
//!
//! A target box with no screen gives no other feedback, so link and lease
//! changes, the first memory fault and a periodic alive line go to the
//! [`BootLog`] behind the `log` sink. A missing log file means the app never
//! started: a firmware boot-policy problem, not a code one.

use core::time::Duration;

use ferrite_efi::bootlog::BootLog;
use ferrite_efi::logger;
use ferrite_hal::Instant;

use ferrite_net::status::MAX_PORTS;
use ferrite_net::{Link, NetStatus};

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
    last_links: heapless::Vec<Link, MAX_PORTS>,
    fault_logged: bool,
    last_alive: Instant,
}

impl Diag {
    #[must_use]
    pub const fn new() -> Self {
        Self { last_links: heapless::Vec::new(), fault_logged: false, last_alive: Instant::from_epoch(Duration::ZERO) }
    }

    /// Log what changed since the last call, and let the log save itself when due.
    pub fn tick(&mut self, now: Instant, net: Option<&NetStatus>, sweep: &SweepStatus) {
        if let Some(net) = net {
            self.links_changed(net);
        }
        if !self.fault_logged
            && let Some(first) = sweep.first_fault
        {
            self.fault_logged = true;
            // Error level saves the log at once, and reaches the loader's link too.
            log::error!(
                "sweep: FAULT at {}: expected {:#018x}, read {:#018x} ({} on cpu{}, pass {})",
                first.fault.addr,
                first.fault.expected,
                first.fault.actual,
                first.test,
                first.cpu,
                sweep.passes
            );
        }
        if now - self.last_alive >= ALIVE_INTERVAL {
            self.last_alive = now;
            match net {
                Some(net) if !net.ports().is_empty() => {
                    log::debug!("alive sweep: {sweep}");
                    net.ports().iter().for_each(|port| log::debug!("alive net: {port}"));
                },
                Some(_) => log::debug!("alive sweep: {sweep} net: no port"),
                None => log::debug!("alive sweep: {sweep}"),
            }
        }
        logger::with(BootLog::poll);
    }

    /// Log each port whose link or lease differs from the last call; ports are matched by address.
    fn links_changed(&mut self, net: &NetStatus) {
        let links: heapless::Vec<Link, MAX_PORTS> = net.ports().iter().map(|port| port.link).collect();
        if links == self.last_links {
            return;
        }
        for link in &links {
            let previous = self.last_links.iter().find(|p| p.mac == link.mac);
            if previous == Some(link) {
                continue;
            }
            let was_up = previous.is_some_and(|p| p.up);
            let had_address = previous.and_then(|p| p.address);
            if link.address.is_some() && link.address != had_address {
                log::info!("net: up, {link}");
            } else if was_up && !link.up {
                log::warn!("net: link lost, {link}");
            } else if had_address.is_some() && link.address.is_none() {
                log::warn!("net: lease lost, {link}");
            } else {
                log::debug!("net: {link}");
            }
        }
        for gone in self.last_links.iter().filter(|p| !links.iter().any(|l| l.mac == p.mac)) {
            log::warn!("net: port gone, {gone}");
        }
        self.last_links = links;
    }
}
