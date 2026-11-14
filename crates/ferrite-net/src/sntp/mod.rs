//! Wall time over the network: an SNTP client (RFC 4330, over the NTP packet of RFC 5905) that keeps
//! the clock right.
//!
//! - [`wire`]: the 48-byte packet and its timestamps, with the 2036 era wrap resolved against the
//!   current clock.
//! - [`exchange`]: one request and its reply. A reply counts only if it is a server's, from the address
//!   asked, with this request's random transmit timestamp echoed, not a kiss-o'-death, from a
//!   synchronized server, with timestamps in order; what it gives is the server's time at the middle of
//!   the exchange and the delay.
//! - [`Sync`]: the schedule. Servers in order (the settings', the DHCP lease's, `pool.ntp.org`), four
//!   samples a server 1.5 s apart of which the least delay is the fix, a resync every 30 minutes and a
//!   backoff from one minute to 30 after a failed round. A server that sends RATE, DENY or RSTR is not
//!   asked again.
//! - The sockets live in each [`crate::Stack`] and are driven by its poll, through whichever port has
//!   an address first, like the notifier's ([`crate::Network::set_sync`] sets it up). A host name is
//!   looked up with the lease's DNS server.
//!
//! The client says what the time is ([`Fix`]); the clock that is corrected by it is the app's
//! (`ferrite_hal::WallClock`).

mod exchange;
mod probe;
mod sync;
#[cfg(test)]
mod tests;
pub mod wire;

pub use self::exchange::{MAX_DELAY, Query, Reject, Sample, best};
pub(crate) use self::probe::Prober;
pub use self::sync::{Failure, Fix, Heard, MAX_SERVERS, QUERY_TIMEOUT, Request, Sync};
