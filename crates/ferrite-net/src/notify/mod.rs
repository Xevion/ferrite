//! Push notifications: a short message sent by POST over plain HTTP to a URL from a config file, in the
//! form ntfy takes (a text body, an optional `Title` header) or as JSON.
//!
//! - [`Config`]: the `.env` lines naming the URL, body format and events.
//! - [`Notifier`]: what an app holds. It says what happened ([`Notifier::notify`]); the queue (bounded,
//!   oldest dropped and counted), the per-attempt timeout and the retries with backoff are its own.
//! - The sockets live in each [`crate::Stack`] and are driven by its poll, through whichever port has
//!   an address first ([`crate::Network::notify`] sets it up). A host name is looked up with the
//!   lease's DNS server; an address is used as it is. HTTPS is not supported.

mod client;
mod config;
mod outbox;
mod request;
#[cfg(test)]
mod tests;

use alloc::vec::Vec;
use core::time::Duration;

use ferrite_hal::Instant;

pub(crate) use self::client::Courier;
pub use self::client::Failure;
pub use self::config::{Config, ConfigError, Endpoint, Event, Events, Format, Host, HostName, Problem};
pub use self::outbox::{Outbox, Outcome, Policy, Settled};
pub use self::request::{Notification, Reply, Verdict, encode, parse_status_line, verdict};

/// What the next attempt needs.
pub(crate) struct Dispatch {
    pub request: Vec<u8>,
    pub host: Host,
    pub port: u16,
}

/// What the notifier has done since it was created.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub delivered: u64,
    /// Dropped from a full queue.
    pub dropped: u64,
    /// Given up on after repeated failures, or refused by the server.
    pub abandoned: u64,
}

pub struct Notifier {
    config: Config,
    outbox: Outbox,
    /// The event of the attempt in progress, for the log.
    sending: Option<Event>,
    delivered: u64,
    abandoned: u64,
}

impl Notifier {
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self::with_policy(config, Policy::default())
    }

    #[must_use]
    pub fn with_policy(config: Config, policy: Policy) -> Self {
        Self { config, outbox: Outbox::new(policy), sending: None, delivered: 0, abandoned: 0 }
    }

    pub const fn config(&self) -> &Config {
        &self.config
    }

    /// Queue `notification` for delivery, unless its event is not one the config asks for. Returns
    /// whether it was queued.
    pub fn notify(&mut self, notification: Notification) -> bool {
        if !self.config.events.contains(notification.event) {
            return false;
        }
        if let Some(dropped) = self.outbox.push(notification) {
            log::warn!(
                "notify: queue full, dropped a {} notification ({} dropped in all)",
                dropped.event,
                self.outbox.dropped()
            );
        }
        true
    }

    /// Notifications not yet delivered or given up on, the one being sent included.
    pub fn pending(&self) -> usize {
        self.outbox.len()
    }

    pub fn stats(&self) -> Stats {
        Stats { delivered: self.delivered, dropped: self.outbox.dropped(), abandoned: self.abandoned }
    }

    pub(crate) const fn attempt_timeout(&self) -> Duration {
        self.outbox.policy().attempt_timeout
    }

    /// Take the next notification due and write its request.
    pub(crate) fn begin(&mut self, now: Instant) -> Option<Dispatch> {
        let notification = self.outbox.begin(now)?;
        self.sending = Some(notification.event);
        Some(Dispatch {
            request: encode(&self.config.endpoint, self.config.format, notification),
            host: self.config.endpoint.host.clone(),
            port: self.config.endpoint.port,
        })
    }

    /// The attempt `begin` started ended with `result`: the status the server replied with, or why
    /// there was none.
    pub(crate) fn finished(&mut self, result: Result<u16, Failure>, now: Instant) {
        let event = self.sending.take();
        let event = event.map_or("a", Event::name);
        let (outcome, failure) = match result {
            Ok(status) => match verdict(status) {
                Verdict::Delivered => (Outcome::Delivered, None),
                Verdict::Retry => (Outcome::Retry, Some(Failure::Status(status))),
                Verdict::Rejected => (Outcome::Rejected, Some(Failure::Status(status))),
            },
            Err(Failure::LinkLost) => {
                self.outbox.cancel();
                return;
            },
            Err(failure) => (Outcome::Retry, Some(failure)),
        };
        match self.outbox.settle(outcome, now) {
            Some(Settled::Delivered) => {
                self.delivered += 1;
                log::info!("notify: {event} delivered, {} waiting", self.outbox.len());
            },
            Some(Settled::Retrying { attempt, after }) => {
                log::warn!("notify: {event} attempt {attempt} failed ({}); retrying in {after:?}", Show(failure));
            },
            Some(Settled::GaveUp { attempts }) => {
                self.abandoned += 1;
                log::warn!("notify: {event} given up after {attempts} attempts ({})", Show(failure));
            },
            Some(Settled::Rejected) => {
                self.abandoned += 1;
                log::warn!("notify: {event} refused ({}); not retrying", Show(failure));
            },
            None => {},
        }
    }

    /// The attempt in progress was lost with its port, through no fault of the notification's.
    pub(crate) fn cancel(&mut self) {
        self.sending = None;
        self.outbox.cancel();
    }
}

struct Show(Option<Failure>);

impl core::fmt::Display for Show {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.0.map_or(Ok(()), |failure| write!(f, "{failure}"))
    }
}
