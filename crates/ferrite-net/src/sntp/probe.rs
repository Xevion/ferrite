//! The sockets of one stack's SNTP request: look the server up if it is a name, send the request from
//! a random port, collect the reply, close. Polled with the stack and never blocking; what to ask and
//! what the answers mean are [`Sync`]'s.

use alloc::vec;
use alloc::vec::Vec;

use ferrite_hal::Instant;
use smoltcp::iface::{Interface, SocketHandle, SocketSet};
use smoltcp::socket::udp;
use smoltcp::wire::{IpAddress, IpEndpoint, Ipv4Address};

use super::sync::{Failure, Heard, QUERY_TIMEOUT, Request, Sync};
use super::wire::{self, PACKET_LEN};
use crate::lookup::{Lookup, Progress};
use crate::notify::Host;
use crate::status::Link;

/// A reply is one packet, and a few more may queue behind a stray.
const PACKETS: usize = 4;

#[derive(Clone, Copy)]
enum Phase {
    Resolving(Lookup),
    Waiting(SocketHandle),
    /// Every socket is already given back.
    Gone,
}

struct Attempt {
    started: Instant,
    port: u16,
    phase: Phase,
}

/// What a poll of an attempt found.
enum Step {
    Pending,
    Settled,
    Failed(Failure),
}

#[derive(Default)]
pub struct Prober {
    attempt: Option<Attempt>,
}

impl Prober {
    /// Whether a request is in flight on this stack.
    pub const fn busy(&self) -> bool {
        self.attempt.is_some()
    }

    /// Advance the request in flight, or start the next one when `sync` has one due and `link` has an
    /// address.
    pub fn poll(
        &mut self,
        iface: &mut Interface,
        sockets: &mut SocketSet<'static>,
        link: &Link,
        sync: Option<&mut Sync>,
        now: Instant,
    ) {
        let Some(sync) = sync else {
            self.abandon(sockets);
            return;
        };
        if self.attempt.is_none() {
            if link.address.is_none() {
                return;
            }
            let lease: Vec<Ipv4Address> = link.ntp.iter().collect();
            let Some(request) = sync.next_request(now, &lease) else { return };
            self.attempt = Self::start(iface, sockets, link, request, sync, now);
        }
        let Some(mut attempt) = self.attempt.take() else { return };
        let step = if link.address.is_none() {
            Self::release(sockets, attempt.phase);
            sync.cancel();
            return;
        } else if now - attempt.started >= QUERY_TIMEOUT {
            Step::Failed(Failure::Timeout)
        } else {
            Self::advance(&mut attempt, sockets, sync, now)
        };
        match step {
            Step::Pending => self.attempt = Some(attempt),
            Step::Settled => Self::release(sockets, attempt.phase),
            Step::Failed(why) => {
                Self::release(sockets, attempt.phase);
                sync.failed(why, now);
            },
        }
    }

    /// Drop the request in flight with its sockets, without telling the client: the port it was on is
    /// gone, and so is the client's wait for it.
    pub fn abandon(&mut self, sockets: &mut SocketSet<'static>) {
        if let Some(attempt) = self.attempt.take() {
            Self::release(sockets, attempt.phase);
        }
    }

    fn start(
        iface: &mut Interface,
        sockets: &mut SocketSet<'static>,
        link: &Link,
        request: Request,
        sync: &mut Sync,
        now: Instant,
    ) -> Option<Attempt> {
        let Request { host, port, .. } = request;
        let phase = match host {
            Host::Addr(ip) => Self::open(sockets, ip, port, sync, now)?,
            Host::Name(name) => match Lookup::start(iface, sockets, link.dns, &name) {
                Ok(lookup) => Phase::Resolving(lookup),
                Err(refused) => {
                    sync.failed(refused.into(), now);
                    return None;
                },
            },
        };
        Some(Attempt { started: now, port, phase })
    }

    /// Send the request to `server`; `None` when the client settled it instead.
    fn open(
        sockets: &mut SocketSet<'static>,
        server: Ipv4Address,
        port: u16,
        sync: &mut Sync,
        now: Instant,
    ) -> Option<Phase> {
        let query = sync.begin(server, now)?;
        let buffer =
            || udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; PACKETS], vec![0; PACKETS * PACKET_LEN]);
        let mut socket = udp::Socket::new(buffer(), buffer());
        let sent = socket.bind(port).ok().and_then(|()| {
            socket.send_slice(&query.packet(), IpEndpoint::new(IpAddress::Ipv4(server), wire::PORT)).ok()
        });
        if sent.is_none() {
            sync.failed(Failure::Socket, now);
            return None;
        }
        Some(Phase::Waiting(sockets.add(socket)))
    }

    fn advance(attempt: &mut Attempt, sockets: &mut SocketSet<'static>, sync: &mut Sync, now: Instant) -> Step {
        match attempt.phase {
            Phase::Resolving(lookup) => match lookup.poll(sockets) {
                Progress::Pending => Step::Pending,
                Progress::Failed => Step::Failed(Failure::Resolve),
                Progress::Found(ip) => {
                    lookup.release(sockets);
                    attempt.phase = Phase::Gone;
                    let Some(phase) = Self::open(sockets, ip, attempt.port, sync, now) else {
                        return Step::Settled;
                    };
                    attempt.phase = phase;
                    Step::Pending
                },
            },
            Phase::Waiting(handle) => {
                let socket = sockets.get_mut::<udp::Socket<'_>>(handle);
                while let Ok((data, meta)) = socket.recv() {
                    let IpAddress::Ipv4(from) = meta.endpoint.addr;
                    if sync.heard(from, data, now) == Heard::Settled {
                        return Step::Settled;
                    }
                }
                Step::Pending
            },
            Phase::Gone => Step::Failed(Failure::Socket),
        }
    }

    fn release(sockets: &mut SocketSet<'static>, phase: Phase) {
        match phase {
            Phase::Resolving(lookup) => lookup.release(sockets),
            Phase::Waiting(handle) => {
                sockets.remove(handle);
            },
            Phase::Gone => {},
        }
    }
}
