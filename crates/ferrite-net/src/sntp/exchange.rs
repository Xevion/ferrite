//! One request and its reply: what the reply must satisfy to be believed, and the time it gives.
//!
//! The origin timestamp is the client's own transmit timestamp echoed back, so it is made unpredictable
//! (its fraction is random) and must come back unchanged: a reply that is not for this request, from
//! an off-path forger or a request long past, cannot match.

use core::fmt;
use core::time::Duration;

use ferrite_hal::{Instant, UnixTime};
use smoltcp::wire::Ipv4Address;

use super::wire::{Kiss, Leap, Mode, Packet, Stratum, Timestamp};

/// The longest round trip a reply may have taken to be used.
///
/// A longer one says the time to no better than half of it, which is worse than the clock's own drift
/// over the minutes between syncs. A busy main loop that polls late makes them.
pub const MAX_DELAY: Duration = Duration::from_secs(1);

/// A request that was sent and awaits its reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Query {
    pub server: Ipv4Address,
    /// The transmit timestamp the request carried.
    pub origin: Timestamp,
    pub sent: Instant,
}

/// What one reply says about the time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    pub server: Ipv4Address,
    pub stratum: u8,
    /// The middle of the exchange on the monotonic clock.
    pub at: Instant,
    /// The server's time at `at`: the middle of its receive and transmit times.
    pub utc: UnixTime,
    /// Time on the network and in the queues: the round trip less the server's own.
    pub delay: Duration,
}

/// Why a reply is not used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reject {
    TooShort,
    /// It came from an address the request did not go to.
    Source,
    /// It is not in server mode, or not version 3 or 4.
    NotAReply,
    /// Its origin timestamp is not the request's transmit timestamp.
    Origin,
    /// A kiss-o'-death.
    Kiss(Kiss),
    /// The server's clock is not synchronized, by the leap indicator or by a stratum of 16 or more.
    Unsynchronized,
    /// A time of zero, or a transmit time before the receive time.
    Timestamps,
    /// A time before 1970.
    Era,
    /// The exchange took longer than [`MAX_DELAY`].
    Delayed,
}

impl Reject {
    /// Whether the reply says nothing about the request that is waiting: a datagram that is not its
    /// reply at all, to be dropped while the wait goes on.
    #[must_use]
    pub const fn is_stray(self) -> bool {
        matches!(self, Self::TooShort | Self::Source | Self::NotAReply | Self::Origin)
    }
}

impl fmt::Display for Reject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort => f.write_str("shorter than an NTP packet"),
            Self::Source => f.write_str("from another address than the server asked"),
            Self::NotAReply => f.write_str("not an NTP server reply"),
            Self::Origin => f.write_str("does not answer this request"),
            Self::Kiss(code) => write!(f, "kiss-o'-death {code}"),
            Self::Unsynchronized => f.write_str("the server's clock is not synchronized"),
            Self::Timestamps => f.write_str("its timestamps are not a time"),
            Self::Era => f.write_str("a time before 1970"),
            Self::Delayed => f.write_str("the exchange took too long to tell the time"),
        }
    }
}

impl Query {
    /// The request packet to send.
    #[must_use]
    pub fn packet(&self) -> [u8; super::wire::PACKET_LEN] {
        Packet::request(self.origin).encode()
    }

    /// The time `bytes`, received from `from` at `received`, gives, when it is a reply to this query that
    /// can be believed. `pivot` is roughly the current UTC, to place the server's timestamps in their
    /// 136-year era.
    pub fn accept(
        &self,
        from: Ipv4Address,
        bytes: &[u8],
        received: Instant,
        pivot: UnixTime,
    ) -> Result<Sample, Reject> {
        let reply = Packet::parse(bytes).map_err(|_| Reject::TooShort)?;
        if from != self.server {
            return Err(Reject::Source);
        }
        if reply.mode != Mode::Server || !matches!(reply.version, 3 | 4) {
            return Err(Reject::NotAReply);
        }
        if reply.origin != self.origin {
            return Err(Reject::Origin);
        }
        let stratum = match reply.stratum {
            Stratum::Kiss => return Err(Reject::Kiss(Kiss::from_code(reply.reference_id))),
            Stratum::Server(n) => n,
            Stratum::Unsynchronized | Stratum::Unknown(_) => return Err(Reject::Unsynchronized),
        };
        if reply.leap == Leap::Alarm {
            return Err(Reject::Unsynchronized);
        }
        if reply.receive.is_zero() || reply.transmit.is_zero() {
            return Err(Reject::Timestamps);
        }
        let to_unix = |stamp: Timestamp| stamp.to_unix(pivot).ok_or(Reject::Era);
        let (t2, t3) = (to_unix(reply.receive)?, to_unix(reply.transmit)?);
        let held = t3.since_epoch().checked_sub(t2.since_epoch()).ok_or(Reject::Timestamps)?;
        let round_trip = received - self.sent;
        if round_trip > MAX_DELAY {
            return Err(Reject::Delayed);
        }
        Ok(Sample {
            server: self.server,
            stratum,
            at: self.sent + round_trip / 2,
            utc: t2.checked_add(held / 2).unwrap_or(t3),
            delay: round_trip.saturating_sub(held),
        })
    }
}

/// The sample with the least delay, which is the one whose time the network disturbed least.
#[must_use]
pub fn best(samples: &[Sample]) -> Option<Sample> {
    samples.iter().copied().min_by_key(|sample| sample.delay)
}
