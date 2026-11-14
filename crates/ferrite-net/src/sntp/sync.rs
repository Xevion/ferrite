//! The SNTP client's schedule: which servers to ask in what order, how many samples to take of each,
//! when to sync again and how long to wait after a failure. It decides and remembers; the sockets that
//! carry its requests are [`super::probe`]'s.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::fmt;
use core::time::Duration;

use ferrite_hal::{Instant, UnixTime};
use smoltcp::wire::Ipv4Address;

use super::exchange::{Query, Reject, Sample, best};
use super::wire::Timestamp;
use crate::lookup::Refused;
use crate::notify::{Host, HostName};

/// Servers the settings may name.
pub const MAX_SERVERS: usize = 4;
/// What is asked last, when nothing before it answers.
const POOL: &str = "pool.ntp.org";
/// Requests to one server in a round, `SPACING` apart; the reply with the least delay is used.
const SAMPLES: u8 = 4;
const SPACING: Duration = Duration::from_millis(1500);
/// How long a request, a lookup included, may take.
pub const QUERY_TIMEOUT: Duration = Duration::from_secs(3);
/// Between a sync and the next.
const RESYNC: Duration = Duration::from_mins(30);
/// After a round that failed, doubling up to the longest.
const BACKOFF_MIN: Duration = Duration::from_mins(1);
const BACKOFF_MAX: Duration = Duration::from_mins(30);
/// How long after getting an address a missing first sync is worth a warning.
const GRACE: Duration = Duration::from_mins(2);
const PORT_BASE: u16 = 49_152;
const PORTS: u64 = 16_384;

/// Why a request got no usable reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    NoDnsServer,
    BadName,
    /// The name did not resolve.
    Resolve,
    /// The socket would not open or send.
    Socket,
    Timeout,
    Rejected(Reject),
    /// The server asked not to be used again.
    Banned,
}

impl From<Refused> for Failure {
    fn from(refused: Refused) -> Self {
        match refused {
            Refused::NoServer => Self::NoDnsServer,
            Refused::BadName => Self::BadName,
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDnsServer => f.write_str("the lease has no DNS server"),
            Self::BadName => f.write_str("the host name was not accepted for a lookup"),
            Self::Resolve => f.write_str("the host name did not resolve"),
            Self::Socket => f.write_str("the request could not be sent"),
            Self::Timeout => f.write_str("no reply in time"),
            Self::Rejected(reject) => write!(f, "reply refused: {reject}"),
            Self::Banned => f.write_str("the server asked not to be used"),
        }
    }
}

/// A sync: the best of the samples one server gave.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fix {
    pub sample: Sample,
    /// How many replies the choice was made from.
    pub samples: usize,
}

/// What the stack's sockets are to ask next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub host: Host,
    pub origin: Timestamp,
    /// The local UDP port to send from.
    pub port: u16,
}

/// What a datagram that arrived while waiting turned out to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Heard {
    /// Not the reply to the request: ignored, and the wait goes on.
    Stray,
    /// The request is settled, as a sample or as a failure.
    Settled,
}

enum Pending {
    /// Handed out, no socket has sent it yet.
    Issued {
        origin: Timestamp,
    },
    Sent(Query),
}

struct Round {
    hosts: Vec<Host>,
    /// The host being asked, in `hosts`.
    at: usize,
    attempts: u8,
    /// Failures in a row with this host.
    misses: u8,
    samples: Vec<Sample>,
    /// Not before.
    next: Instant,
    pending: Option<Pending>,
}

enum Phase {
    Waiting { until: Instant },
    Round(Round),
}

/// The client's state.
pub struct Sync {
    servers: Vec<Host>,
    pool: Option<Host>,
    entropy: Box<dyn FnMut() -> u64>,
    /// About the current UTC, to place a server's timestamps in their era.
    pivot: UnixTime,
    banned: Vec<Ipv4Address>,
    phase: Phase,
    fix: Option<Fix>,
    leased_since: Option<Instant>,
    synced_once: bool,
    rounds_failed: u32,
    retry: Duration,
    warned: bool,
    last_failure: Option<Failure>,
}

impl Sync {
    /// A client that asks `servers` first (at most [`MAX_SERVERS`]), then the ones a lease names, then
    /// the public pool. `entropy` makes the transmit timestamps and ports unpredictable. `pivot` is the
    /// best guess of the current time until [`Sync::set_pivot`] says better.
    #[must_use]
    pub fn new(mut servers: Vec<Host>, entropy: Box<dyn FnMut() -> u64>, pivot: UnixTime) -> Self {
        servers.truncate(MAX_SERVERS);
        Self {
            servers,
            pool: HostName::new(POOL).map(Host::Name),
            entropy,
            pivot,
            banned: Vec::new(),
            phase: Phase::Waiting { until: Instant::default() },
            fix: None,
            leased_since: None,
            synced_once: false,
            rounds_failed: 0,
            retry: BACKOFF_MIN,
            warned: false,
            last_failure: None,
        }
    }

    /// The current time as the clock has it, which the era of a timestamp is chosen by.
    pub const fn set_pivot(&mut self, utc: UnixTime) {
        self.pivot = utc;
    }

    /// The servers the client is set to use, before the lease's and the pool.
    #[must_use]
    pub fn servers(&self) -> &[Host] {
        &self.servers
    }

    /// The sync a round finished with, once.
    pub const fn take_fix(&mut self) -> Option<Fix> {
        self.fix.take()
    }

    /// Whether a stack should be carrying requests: one is under way or a round is due.
    #[must_use]
    pub fn wants_port(&self, now: Instant) -> bool {
        match &self.phase {
            Phase::Round(_) => true,
            Phase::Waiting { until } => now >= *until,
        }
    }

    /// Note whether any port has an address, and warn once when none of the first syncs has worked
    /// two minutes after one had.
    pub fn observe(&mut self, now: Instant, leased: bool) {
        if leased {
            self.leased_since.get_or_insert(now);
        } else {
            self.leased_since = None;
        }
        let overdue = self.leased_since.is_some_and(|since| now - since >= GRACE);
        if overdue && !self.synced_once && !self.warned {
            self.warned = true;
            match self.last_failure {
                Some(why) => log::warn!(
                    "time: no NTP sync {GRACE:?} after getting an address, {} rounds failed, the last: {why}; the clock stays on the firmware's",
                    self.rounds_failed
                ),
                None => log::warn!(
                    "time: no NTP sync {GRACE:?} after getting an address; the clock stays on the firmware's"
                ),
            }
        }
    }

    /// The next request, when one is due. `lease` are the NTP servers of the DHCP lease of the port
    /// that will send it.
    pub fn next_request(&mut self, now: Instant, lease: &[Ipv4Address]) -> Option<Request> {
        if let Phase::Waiting { until } = self.phase {
            if now < until {
                return None;
            }
            let hosts = self.hosts(lease);
            self.phase = Phase::Round(Round {
                hosts,
                at: 0,
                attempts: 0,
                misses: 0,
                samples: Vec::new(),
                next: now,
                pending: None,
            });
        }
        let Phase::Round(round) = &mut self.phase else { return None };
        if round.pending.is_some() || now < round.next {
            return None;
        }
        let host = round.hosts.get(round.at)?.clone();
        let random = (self.entropy)();
        let origin = Timestamp::new(Timestamp::from_unix_seconds(self.pivot).seconds(), random as u32);
        round.pending = Some(Pending::Issued { origin });
        Some(Request { host, origin, port: PORT_BASE + ((random >> 32) % PORTS) as u16 })
    }

    /// The request is going to `server` now. `None` when that server is one not to be used, which
    /// settles the request.
    pub fn begin(&mut self, server: Ipv4Address, now: Instant) -> Option<Query> {
        let banned = self.banned.contains(&server);
        let Phase::Round(round) = &mut self.phase else { return None };
        let Some(Pending::Issued { origin }) = round.pending else { return None };
        if banned {
            self.settle(Err(Failure::Banned), true, now);
            return None;
        }
        let query = Query { server, origin, sent: now };
        round.pending = Some(Pending::Sent(query));
        Some(query)
    }

    /// A datagram arrived from `from` for the request in flight.
    pub fn heard(&mut self, from: Ipv4Address, bytes: &[u8], now: Instant) -> Heard {
        let Phase::Round(round) = &self.phase else { return Heard::Stray };
        let Some(Pending::Sent(query)) = round.pending else { return Heard::Stray };
        match query.accept(from, bytes, now, self.pivot) {
            Ok(sample) => self.settle(Ok(sample), false, now),
            Err(reject) if reject.is_stray() => {
                log::debug!("time: dropped a datagram from {from}: {reject}");
                return Heard::Stray;
            },
            Err(reject @ Reject::Kiss(code)) => {
                if code.bans() {
                    self.banned.push(query.server);
                    log::warn!("time: {} sent a kiss-o'-death {code}, and is not used again", query.server);
                }
                self.settle(Err(Failure::Rejected(reject)), code.bans(), now);
            },
            Err(reject) => {
                log::debug!("time: {} gave a reply that is not used: {reject}", query.server);
                self.settle(Err(Failure::Rejected(reject)), false, now);
            },
        }
        Heard::Settled
    }

    /// The request in flight failed for `why`.
    pub fn failed(&mut self, why: Failure, now: Instant) {
        let hopeless = matches!(why, Failure::NoDnsServer | Failure::BadName | Failure::Resolve);
        self.settle(Err(why), hopeless, now);
    }

    /// The request in flight was lost with its port, through no fault of the server's.
    pub fn cancel(&mut self) {
        if let Phase::Round(round) = &mut self.phase {
            round.pending = None;
        }
    }

    /// The hosts of a round: the settings', the lease's, the pool, each once.
    fn hosts(&self, lease: &[Ipv4Address]) -> Vec<Host> {
        let mut hosts: Vec<Host> = Vec::new();
        let all = self.servers.iter().cloned().chain(lease.iter().copied().map(Host::Addr)).chain(self.pool.clone());
        for host in all {
            let unusable = matches!(&host, Host::Addr(ip) if self.banned.contains(ip));
            if !unusable && !hosts.contains(&host) {
                hosts.push(host);
            }
        }
        hosts
    }

    /// An attempt ended with a sample or a failure. `done_with_host`: ask no more of this host this round.
    fn settle(&mut self, outcome: Result<Sample, Failure>, done_with_host: bool, now: Instant) {
        let Phase::Round(round) = &mut self.phase else { return };
        round.pending = None;
        round.attempts += 1;
        round.next = now + SPACING;
        match outcome {
            Ok(sample) => {
                round.misses = 0;
                round.samples.push(sample);
            },
            Err(why) => {
                round.misses += 1;
                self.last_failure = Some(why);
            },
        }
        let hopeless = round.samples.is_empty() && round.misses >= 2;
        if !(done_with_host || hopeless || round.attempts >= SAMPLES) {
            return;
        }
        if let Some(sample) = best(&round.samples) {
            let fix = Fix { sample, samples: round.samples.len() };
            self.succeed(fix, now);
            return;
        }
        round.at += 1;
        round.attempts = 0;
        round.misses = 0;
        round.next = now;
        if round.at >= round.hosts.len() {
            self.fail_round(now);
        }
    }

    fn succeed(&mut self, fix: Fix, now: Instant) {
        self.fix = Some(fix);
        self.synced_once = true;
        self.rounds_failed = 0;
        self.retry = BACKOFF_MIN;
        self.warned = false;
        self.last_failure = None;
        self.phase = Phase::Waiting { until: now + RESYNC };
    }

    fn fail_round(&mut self, now: Instant) {
        self.rounds_failed += 1;
        let wait = self.retry;
        self.retry = (self.retry * 2).min(BACKOFF_MAX);
        self.phase = Phase::Waiting { until: now + wait };
        if self.synced_once && !self.warned {
            self.warned = true;
            let why = self.last_failure.map_or_else(|| alloc::string::String::from("-"), |why| alloc::format!("{why}"));
            log::warn!("time: NTP sync failed ({why}); the clock runs on its own, the next try is in {wait:?}");
        } else {
            log::debug!("time: round {} failed, the next try is in {wait:?}", self.rounds_failed);
        }
    }

    #[cfg(test)]
    pub(super) const fn warned(&self) -> bool {
        self.warned
    }
}
