//! The sockets of one stack's notification attempt: look the host up, connect, send the request, read
//! the status line, close. It is polled with the stack and never blocks; the attempt's state is one
//! small enum that each poll looks at once.

use alloc::vec;
use alloc::vec::Vec;
use core::fmt;
use core::time::Duration;

use ferrite_hal::Instant;
use smoltcp::iface::{Interface, SocketHandle, SocketSet};
use smoltcp::socket::tcp;
use smoltcp::wire::{IpAddress, Ipv4Address};

use super::config::Host;
use super::request::{Reply, parse_status_line};
use super::{Dispatch, Notifier};
use crate::lookup::{Lookup, Progress, Refused};
use crate::status::Link;

/// The reply is only read as far as its status line.
const REPLY_BUFFER: usize = 512;
const REPLY_MAX: usize = 256;
const LOCAL_PORT_BASE: u16 = 49_152;
const LOCAL_PORTS: u16 = 8_192;
/// How long a closed connection may take to finish before it is cut.
const CLOSE_GRACE: Duration = Duration::from_secs(3);

/// Why an attempt failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    NoDnsServer,
    BadName,
    Resolve,
    Connect,
    Send,
    Timeout,
    /// The port lost its address mid-attempt.
    LinkLost,
    ClosedEarly,
    BadReply,
    Status(u16),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDnsServer => f.write_str("the lease has no DNS server"),
            Self::BadName => f.write_str("the host name was not accepted for a lookup"),
            Self::Resolve => f.write_str("the host name did not resolve"),
            Self::Connect => f.write_str("the connection was refused or reset"),
            Self::Send => f.write_str("the request could not be sent"),
            Self::Timeout => f.write_str("timed out"),
            Self::LinkLost => f.write_str("the port lost its address"),
            Self::ClosedEarly => f.write_str("the server closed without a status line"),
            Self::BadReply => f.write_str("the reply was not HTTP"),
            Self::Status(status) => write!(f, "HTTP {status}"),
        }
    }
}

#[derive(Clone, Copy)]
enum Phase {
    Resolving(Lookup),
    /// Connecting, then sending the request.
    Sending {
        tcp: SocketHandle,
    },
    Reading {
        tcp: SocketHandle,
    },
    /// Every socket is already given back.
    Gone,
}

struct Attempt {
    started: Instant,
    phase: Phase,
    request: Vec<u8>,
    port: u16,
    sent: usize,
    reply: Vec<u8>,
}

/// What a poll of an attempt found.
enum Step {
    Pending,
    Replied(u16),
    Failed(Failure),
}

pub struct Courier {
    attempt: Option<Attempt>,
    /// Connections shut down, until they finish or are cut.
    closing: Vec<(SocketHandle, Instant)>,
    next_port: u16,
}

impl Courier {
    pub const fn new(seed: u64) -> Self {
        Self { attempt: None, closing: Vec::new(), next_port: LOCAL_PORT_BASE + (seed % LOCAL_PORTS as u64) as u16 }
    }

    /// Whether an attempt is running on this stack.
    pub const fn busy(&self) -> bool {
        self.attempt.is_some()
    }

    /// Advance the attempt, or start the next one when `notifier` has one due and `link` has an
    /// address. Without a `notifier` only the finished connections are cleaned up.
    pub fn poll(
        &mut self,
        iface: &mut Interface,
        sockets: &mut SocketSet<'static>,
        link: &Link,
        notifier: Option<&mut Notifier>,
        now: Instant,
    ) {
        self.reap(sockets, now);
        let Some(notifier) = notifier else { return };
        if self.attempt.is_none() {
            if link.address.is_none() {
                return;
            }
            let Some(dispatch) = notifier.begin(now) else { return };
            match self.start(iface, sockets, link, dispatch, now) {
                Ok(attempt) => self.attempt = Some(attempt),
                Err(failure) => notifier.finished(Err(failure), now),
            }
        }
        let Some(mut attempt) = self.attempt.take() else { return };
        let step = if now - attempt.started >= notifier.attempt_timeout() {
            Step::Failed(Failure::Timeout)
        } else if link.address.is_none() {
            Step::Failed(Failure::LinkLost)
        } else {
            self.advance(&mut attempt, iface, sockets)
        };
        match step {
            Step::Pending => self.attempt = Some(attempt),
            Step::Replied(status) => {
                self.release(sockets, attempt.phase, now);
                notifier.finished(Ok(status), now);
            },
            Step::Failed(failure) => {
                self.release(sockets, attempt.phase, now);
                notifier.finished(Err(failure), now);
            },
        }
    }

    fn start(
        &mut self,
        iface: &mut Interface,
        sockets: &mut SocketSet<'static>,
        link: &Link,
        dispatch: Dispatch,
        now: Instant,
    ) -> Result<Attempt, Failure> {
        let Dispatch { request, host, port } = dispatch;
        let phase = match host {
            Host::Addr(ip) => Phase::Sending { tcp: self.connect(iface, sockets, ip, port, request.len())? },
            Host::Name(name) => {
                let lookup = Lookup::start(iface, sockets, link.dns, &name).map_err(|refused| match refused {
                    Refused::NoServer => Failure::NoDnsServer,
                    Refused::BadName => Failure::BadName,
                })?;
                Phase::Resolving(lookup)
            },
        };
        Ok(Attempt { started: now, phase, request, port, sent: 0, reply: Vec::with_capacity(REPLY_MAX) })
    }

    fn connect(
        &mut self,
        iface: &mut Interface,
        sockets: &mut SocketSet<'static>,
        ip: Ipv4Address,
        port: u16,
        request_len: usize,
    ) -> Result<SocketHandle, Failure> {
        let mut socket = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0; REPLY_BUFFER]),
            tcp::SocketBuffer::new(vec![0; request_len.max(64)]),
        );
        let local = self.next_port;
        self.next_port = LOCAL_PORT_BASE + (local - LOCAL_PORT_BASE + 1) % LOCAL_PORTS;
        socket.connect(iface.context(), (IpAddress::Ipv4(ip), port), local).map_err(|_| Failure::Connect)?;
        Ok(sockets.add(socket))
    }

    fn advance(&mut self, attempt: &mut Attempt, iface: &mut Interface, sockets: &mut SocketSet<'static>) -> Step {
        match attempt.phase {
            Phase::Resolving(lookup) => {
                let ip = match lookup.poll(sockets) {
                    Progress::Found(ip) => ip,
                    Progress::Pending => return Step::Pending,
                    Progress::Failed => return Step::Failed(Failure::Resolve),
                };
                lookup.release(sockets);
                match self.connect(iface, sockets, ip, attempt.port, attempt.request.len()) {
                    Ok(tcp) => {
                        attempt.phase = Phase::Sending { tcp };
                        Step::Pending
                    },
                    Err(failure) => {
                        attempt.phase = Phase::Gone;
                        Step::Failed(failure)
                    },
                }
            },
            Phase::Sending { tcp } => {
                let socket = sockets.get_mut::<tcp::Socket<'_>>(tcp);
                if matches!(socket.state(), tcp::State::Closed | tcp::State::TimeWait) {
                    return Step::Failed(Failure::Connect);
                }
                if !socket.may_send() {
                    return Step::Pending;
                }
                match socket.send_slice(&attempt.request[attempt.sent..]) {
                    Ok(n) => attempt.sent += n,
                    Err(_) => return Step::Failed(Failure::Send),
                }
                if attempt.sent == attempt.request.len() {
                    attempt.phase = Phase::Reading { tcp };
                }
                Step::Pending
            },
            Phase::Reading { tcp } => {
                let socket = sockets.get_mut::<tcp::Socket<'_>>(tcp);
                let mut chunk = [0_u8; 64];
                while socket.can_recv() && attempt.reply.len() < REPLY_MAX {
                    match socket.recv_slice(&mut chunk) {
                        Ok(n) => attempt.reply.extend_from_slice(&chunk[..n]),
                        Err(_) => break,
                    }
                }
                match parse_status_line(&attempt.reply) {
                    Reply::Status(status) => Step::Replied(status),
                    Reply::Malformed => Step::Failed(Failure::BadReply),
                    Reply::Incomplete if !socket.may_recv() && !socket.can_recv() => Step::Failed(Failure::ClosedEarly),
                    Reply::Incomplete => Step::Pending,
                }
            },
            Phase::Gone => Step::Failed(Failure::Connect),
        }
    }

    /// Give back an attempt's sockets: a lookup's at once, a connection's once its close finishes.
    fn release(&mut self, sockets: &mut SocketSet<'static>, phase: Phase, now: Instant) {
        match phase {
            Phase::Resolving(lookup) => lookup.release(sockets),
            Phase::Sending { tcp } | Phase::Reading { tcp } => {
                sockets.get_mut::<tcp::Socket<'_>>(tcp).close();
                self.closing.push((tcp, now));
            },
            Phase::Gone => {},
        }
    }

    fn reap(&mut self, sockets: &mut SocketSet<'static>, now: Instant) {
        self.closing.retain(|&(handle, since)| {
            let socket = sockets.get_mut::<tcp::Socket<'_>>(handle);
            let done =
                matches!(socket.state(), tcp::State::Closed | tcp::State::TimeWait) || now - since >= CLOSE_GRACE;
            if done {
                socket.abort();
                sockets.remove(handle);
            }
            !done
        });
    }
}
