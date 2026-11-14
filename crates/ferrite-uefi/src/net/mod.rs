//! The app's side of networking: what its status page serves and which ports it runs it on.
//!
//! The stacks, DHCP, mDNS, ICMP echo and the HTTP server are `ferrite_net`'s, one stack per port
//! and any number at once. This module is the page, its JSON document and controls ([`Console`]),
//! and the choice of ports: every wired NIC the firmware drives (`ferrite_efi::snp`).

mod api;
mod json;
#[cfg(test)]
mod testing;

use alloc::string::String;
use core::time::Duration;

use ferrite_efi::clock::Clock;
use ferrite_efi::entropy;
use ferrite_efi::snp::{self, SnpError};
use ferrite_hal::{Clock as _, Instant};
use ferrite_net::http::{Rejection, Route, Site};
use ferrite_net::{NetStatus, Network, StackConfig};

use self::api::{BodyError, Control};
use self::json::{StatusDoc, ToJson as _};
use crate::control::{Command, Commands};
use crate::report::Snapshot;
use crate::sweep::SweepStatus;

static PAGE: &str = include_str!("page.html");

/// Why there is no port, when no wired NIC was opened and nothing says otherwise.
const NO_NIC: &str = "no wired NIC driven by the firmware";

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
}

impl Net {
    /// Open every wired NIC the firmware drives as a port, each with its own stack.
    pub fn setup(clock: Clock) -> Self {
        let now = clock.now();
        let mut net = Self { network: Network::new(Console).timed(clock), failure: None };
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
            self.network.add(alloc::boxed::Box::new(port), config, now);
        }
    }

    /// Adds a stack for each Wi-Fi port that has come up and removes the one whose port went away.
    /// Called every poll, so a station that connects later gets its stack like a wired NIC does.
    #[expect(
        clippy::unused_self,
        clippy::needless_pass_by_ref_mut,
        reason = "the hook Wi-Fi ports are added and removed through"
    )]
    fn sync_wifi(&mut self, _now: Instant) {}

    /// Advance every stack to `now`. Returns how long may pass before the next poll, or `None`
    /// when there is no port. Requests the status page makes are queued on `commands`.
    pub fn poll(&mut self, now: Instant, snapshot: &Snapshot<'_>, commands: &mut Commands) -> Option<Duration> {
        self.sync_wifi(now);
        self.network.poll(now, snapshot, &mut |command| commands.push(command))
    }

    pub fn status(&self) -> NetStatus {
        self.network.status()
    }

    /// Why there is no port, for the screen.
    pub fn reason(&self) -> &'static str {
        self.failure.as_ref().map_or(NO_NIC, SnpError::reason)
    }
}

#[cfg(test)]
mod tests;
