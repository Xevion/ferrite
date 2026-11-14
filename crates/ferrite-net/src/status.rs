//! What a stack reports about its port, for the screen, the log and the page.

use core::fmt;

use smoltcp::wire::{Ipv4Address, Ipv4Cidr};

use crate::port::{MacAddr, Medium, PortStats};

/// Ports a [`NetStatus`] describes; a network with more reports the first of them.
pub const MAX_PORTS: usize = 8;

/// An optional address, shown as `-` when absent.
pub struct OptAddr(pub Option<Ipv4Address>);

impl fmt::Display for OptAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(addr) => write!(f, "{addr}"),
            None => f.write_str("-"),
        }
    }
}

/// Link and lease state: what changes rarely and is worth logging when it does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Link {
    pub medium: Medium,
    pub up: bool,
    pub mac: MacAddr,
    pub address: Option<Ipv4Cidr>,
    pub gateway: Option<Ipv4Address>,
    pub dns: Option<Ipv4Address>,
}

impl fmt::Display for Link {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} mac={} link={} ip=", self.medium, self.mac, if self.up { "up" } else { "down" })?;
        match self.address {
            Some(cidr) => write!(f, "{cidr}")?,
            None => f.write_str("-")?,
        }
        write!(f, " gw={} dns={}", OptAddr(self.gateway), OptAddr(self.dns))
    }
}

/// What the stack's services have done since it was set up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ServiceCounters {
    /// mDNS queries (QR clear) received, for any name.
    pub mdns_queries: u32,
    pub mdns_replies: u32,
    pub mdns_malformed: u32,
    pub http_responses: u32,
}

/// One port's link, frames and services.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PortStatus {
    pub link: Link,
    pub frames: PortStats,
    pub services: ServiceCounters,
}

impl fmt::Display for PortStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = &self.services;
        write!(
            f,
            "{} mdns_queries={} mdns_replies={} mdns_malformed={} http={} {}",
            self.link, s.mdns_queries, s.mdns_replies, s.mdns_malformed, s.http_responses, self.frames
        )
    }
}

/// Every port of a network, as of one poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetStatus {
    len: usize,
    ports: [PortStatus; MAX_PORTS],
    /// Ports beyond [`MAX_PORTS`] that are not described.
    pub omitted: usize,
}

impl Default for NetStatus {
    fn default() -> Self {
        Self { len: 0, ports: [PortStatus::default(); MAX_PORTS], omitted: 0 }
    }
}

impl NetStatus {
    pub fn push(&mut self, port: PortStatus) {
        match self.ports.get_mut(self.len) {
            Some(slot) => {
                *slot = port;
                self.len += 1;
            },
            None => self.omitted += 1,
        }
    }

    pub fn ports(&self) -> &[PortStatus] {
        &self.ports[..self.len]
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether any port's link or lease differs from `other`'s, traffic aside.
    pub fn links_differ(&self, other: &Self) -> bool {
        self.len != other.len || self.ports().iter().zip(other.ports()).any(|(a, b)| a.link != b.link)
    }
}
