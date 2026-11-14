//! The network foundation every app shares, platform-free.
//!
//! - [`port`]: the [`Port`] every NIC is seen through (the firmware's SNP, a Wi-Fi station's netdev
//!   queues), and [`QueuePort`], the port for a device that pushes frames.
//! - [`Stack`]: the IP stack of one port, with DHCP, mDNS, ICMP echo and the HTTP server.
//! - [`Network`]: any number of stacks polled together, ports added and removed at any time.
//! - [`notify`]: push notifications sent by POST over HTTP, queued with retries and sent through the first port
//!   with an address.
//! - [`sntp`]: the clock kept right by an SNTP client, asking through the first port with an address
//!   too.
//! - [`http::Site`]: what an app implements to be served, over every stack at once.

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod device;
mod dhcp;
pub mod http;
mod lookup;
pub mod loopback;
pub mod mdns;
mod network;
pub mod notify;
pub mod ping;
pub mod port;
mod queue;
pub mod sntp;
mod stack;
pub mod status;
#[cfg(test)]
mod testing;
#[cfg(test)]
mod tests;

pub use self::network::{Network, PortId};
pub use self::port::{MacAddr, Medium, Port, PortStats, TxError};
pub use self::queue::QueuePort;
pub use self::stack::{PollCtx, Stack, StackConfig};
pub use self::status::{Link, MAX_NTP_SERVERS, NetStatus, NtpServers, OptAddr, PortStatus, ServiceCounters};
