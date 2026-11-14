//! The app's side of networking: what its status page serves and which ports it runs it on.
//!
//! The stacks, DHCP, mDNS, ICMP echo and the HTTP server are `ferrite_net`'s, one stack per port
//! and any number at once. This module is the page, its JSON document and controls ([`Console`]),
//! and the choice of ports: every wired NIC the firmware drives (`ferrite_efi::snp`) and the Wi-Fi
//! station's link once it is up ([`wifi`]).

mod api;
mod json;
#[cfg(test)]
mod testing;
mod wifi;

use alloc::boxed::Box;
use alloc::string::String;
use core::fmt;
use core::time::Duration;

use ferrite_efi::clock::Clock;
use ferrite_efi::entropy;
use ferrite_efi::snp::{self, SnpError};
use ferrite_hal::{Clock as _, Instant};
use ferrite_net::http::{Rejection, Route, Site};
use ferrite_net::{NetStatus, Network, PortId, StackConfig};
use ferrite_wifi::PortEvent;

use self::api::{BodyError, Control};
use self::json::{StatusDoc, ToJson as _};
use self::wifi::Wifi;
use crate::control::{Command, Commands};
use crate::report::Snapshot;
use crate::sweep::SweepStatus;

static PAGE: &str = include_str!("page.html");

/// Why there is no port, when no wired NIC was opened and nothing says otherwise.
const NO_NIC: &str = "no wired NIC driven by the firmware";

/// Why no port is up: the wired side's reason and what the Wi-Fi station is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoNetwork {
    pub wired: &'static str,
    pub wifi: &'static str,
}

impl fmt::Display for NoNetwork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}; Wi-Fi: {}", self.wired, self.wifi)
    }
}

/// What the status document shows besides the clock and the traffic counters.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Key {
    status: SweepStatus,
    /// Moves whenever the finished tests, passes or processors do.
    revision: u64,
    /// The fault log's length, since the status counts faults but not the log's contents.
    faults: usize,
}

/// The status page, its API and the run's controls.
pub struct Console;

impl Site for Console {
    type State<'a> = Snapshot<'a>;
    type Control = Control;
    type Command = Command;
    type BodyError = BodyError;
    type Key = Key;

    fn page(&self) -> &'static str {
        PAGE
    }

    fn route(&self, method: &str, path: &str) -> Result<Route<Control>, Rejection> {
        api::resolve(method, path)
    }

    fn command(&self, control: Control, body: &[u8]) -> Result<Command, BodyError> {
        api::command(control, body)
    }

    fn ends_run(command: &Command) -> bool {
        *command == Command::Quit
    }

    fn key(&self, snapshot: &Snapshot<'_>) -> Key {
        Key { status: snapshot.status, revision: snapshot.history.revision, faults: snapshot.faults.len() }
    }

    fn render(&self, snapshot: &Snapshot<'_>, net: &NetStatus, now: Instant, out: &mut String) {
        StatusDoc { snapshot, net, now }.to_json(out);
    }
}

pub struct Net {
    network: Network<Console>,
    /// Why no wired NIC opened, when none did.
    failure: Option<SnpError>,
    wifi: Wifi,
    /// The stack over the Wi-Fi link, while the station offers one.
    wifi_port: Option<PortId>,
}

impl Net {
    /// Open every wired NIC the firmware drives as a port, each with its own stack, and start the
    /// Wi-Fi station, whose link joins as one more port once it is up. Call before the sweep claims
    /// memory.
    pub fn setup(clock: Clock) -> Self {
        let now = clock.now();
        let mut net = Self {
            network: Network::new(Console).timed(clock),
            failure: None,
            wifi: Wifi::start(clock),
            wifi_port: None,
        };
        net.open_wired(now);
        net
    }

    fn open_wired(&mut self, now: Instant) {
        let found = match snp::discover() {
            Ok(found) => found,
            Err(e) => {
                log::warn!("net: {e}");
                self.failure = Some(e);
                return;
            },
        };
        for failure in &found.failures {
            if found.ports.is_empty() {
                log::warn!("net: a wired NIC was not opened: {failure}");
            } else {
                log::debug!("net: an SNP handle was not opened: {failure}");
            }
        }
        self.failure = found.failures.into_iter().next();
        for port in found.ports {
            let config = StackConfig::appliance(entropy::random_u64());
            self.network.add(Box::new(port), config, now);
        }
    }

    /// Give the Wi-Fi link its own stack when the station offers one, and drop it when the link goes.
    /// A reconnect is a new port with the same address, so its stack starts afresh with a new lease.
    fn sync_wifi(&mut self, now: Instant) {
        while let Some(event) = self.wifi.next_port_event() {
            if let Some(id) = self.wifi_port.take() {
                self.network.remove(id);
                log::info!("wifi: link down; its stack is dropped");
            }
            if let PortEvent::Up(port) = event {
                let config = StackConfig::appliance(entropy::random_u64());
                self.wifi_port = Some(self.network.add(Box::new(port), config, now));
                log::info!("wifi: link up; starting DHCP over it");
            }
        }
    }

    /// Advance the Wi-Fi station and every stack to `now`. Returns how long may pass before the next
    /// poll, or `None` when nothing is waiting. Requests the status page makes are queued on `commands`.
    pub fn poll(&mut self, now: Instant, snapshot: &Snapshot<'_>, commands: &mut Commands) -> Option<Duration> {
        let wifi = self.wifi.poll();
        self.sync_wifi(now);
        let stacks = self.network.poll(now, snapshot, &mut |command| commands.push(command));
        match (wifi, stacks) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    pub fn status(&self) -> NetStatus {
        self.network.status()
    }

    /// Why there is no port, for the screen.
    pub fn reason(&self) -> NoNetwork {
        NoNetwork { wired: self.failure.as_ref().map_or(NO_NIC, SnpError::reason), wifi: self.wifi.label() }
    }

    /// Leave the Wi-Fi network and give its card and memory back; the wired ports close on drop.
    pub fn shutdown(&mut self) {
        if let Some(id) = self.wifi_port.take() {
            self.network.remove(id);
        }
        self.wifi.shutdown();
    }
}

#[cfg(test)]
mod tests;
