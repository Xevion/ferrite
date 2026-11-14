//! ICMP echo requests to one IPv4 address through a smoltcp ICMP socket, with the round trip of each
//! reply logged.

use alloc::vec;
use alloc::vec::Vec;
use core::time::Duration;

use ferrite_hal::Instant;
use smoltcp::iface::{SocketHandle, SocketSet};
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::socket::icmp;
use smoltcp::wire::{Icmpv4Packet, Icmpv4Repr, IpAddress, Ipv4Address};

const PAYLOAD: &[u8] = b"ferrite ping";
const BUFFER: usize = 512;
const PACKETS: usize = 4;

/// One echo request's fate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Reply(Duration),
    Lost,
}

pub struct Pinger {
    handle: SocketHandle,
    target: Ipv4Address,
    ident: u16,
    count: u16,
    interval: Duration,
    timeout: Duration,
    next_seq: u16,
    next_at: Instant,
    /// Requests sent and not yet answered or given up, with when they went out.
    waiting: Vec<(u16, Instant)>,
    outcomes: Vec<Outcome>,
}

impl Pinger {
    /// Send `count` echo requests to `target`, `interval` apart, the first at once; each waits at most
    /// `timeout` for its reply.
    pub fn new(
        sockets: &mut SocketSet<'static>,
        target: Ipv4Address,
        ident: u16,
        count: u16,
        interval: Duration,
        timeout: Duration,
        now: Instant,
    ) -> Self {
        let buffer = || icmp::PacketBuffer::new(vec![icmp::PacketMetadata::EMPTY; PACKETS], vec![0; BUFFER]);
        let mut socket = icmp::Socket::new(buffer(), buffer());
        if let Err(error) = socket.bind(icmp::Endpoint::Ident(ident)) {
            log::warn!("ping {target}: binding ident {ident:#x} failed: {error}");
        }
        let handle = sockets.add(socket);
        Self {
            handle,
            target,
            ident,
            count,
            interval,
            timeout,
            next_seq: 0,
            next_at: now,
            waiting: Vec::new(),
            outcomes: Vec::new(),
        }
    }

    /// Send what is due, take replies and give up on requests past their timeout.
    pub fn poll(&mut self, sockets: &mut SocketSet<'static>, now: Instant) {
        let socket = sockets.get_mut::<icmp::Socket<'_>>(self.handle);
        let checksum = ChecksumCapabilities::default();
        while socket.can_recv() {
            let Ok((payload, from)) = socket.recv() else { break };
            let Ok(packet) = Icmpv4Packet::new_checked(payload) else { continue };
            let Ok(Icmpv4Repr::EchoReply { ident, seq_no, .. }) = Icmpv4Repr::parse(&packet, &checksum) else {
                continue;
            };
            let Some(at) = self.waiting.iter().position(|(seq, _)| *seq == seq_no).filter(|_| ident == self.ident)
            else {
                continue;
            };
            let (_, sent) = self.waiting.swap_remove(at);
            let rtt = now - sent;
            log::info!("ping {}: reply from {from} seq {seq_no} after {} us", self.target, rtt.as_micros());
            self.outcomes.push(Outcome::Reply(rtt));
        }
        let target = self.target;
        let timeout = self.timeout;
        let outcomes = &mut self.outcomes;
        self.waiting.retain(|(seq, sent)| {
            let alive = now - *sent < timeout;
            if !alive {
                log::warn!("ping {target}: no reply to seq {seq} within {} ms", timeout.as_millis());
                outcomes.push(Outcome::Lost);
            }
            alive
        });
        if self.next_seq < self.count && now >= self.next_at && socket.can_send() {
            let repr = Icmpv4Repr::EchoRequest { ident: self.ident, seq_no: self.next_seq, data: PAYLOAD };
            match socket.send(repr.buffer_len(), IpAddress::Ipv4(self.target)) {
                Ok(buf) => {
                    repr.emit(&mut Icmpv4Packet::new_unchecked(buf), &checksum);
                    self.waiting.push((self.next_seq, now));
                },
                Err(error) => {
                    log::warn!("ping {}: queueing seq {} failed: {error}", self.target, self.next_seq);
                    self.outcomes.push(Outcome::Lost);
                },
            }
            self.next_seq += 1;
            self.next_at = now + self.interval;
        }
    }

    /// Every request was sent and has a reply or was given up.
    pub fn done(&self) -> bool {
        self.next_seq == self.count && self.waiting.is_empty()
    }

    pub fn replies(&self) -> impl Iterator<Item = Duration> + '_ {
        self.outcomes.iter().filter_map(|outcome| match outcome {
            Outcome::Reply(rtt) => Some(*rtt),
            Outcome::Lost => None,
        })
    }

    pub fn target(&self) -> Ipv4Address {
        self.target
    }

    /// Drop the socket from `sockets`.
    pub fn remove(self, sockets: &mut SocketSet<'static>) {
        sockets.remove(self.handle);
    }
}
