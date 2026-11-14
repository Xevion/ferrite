//! The wire format against a real exchange, the checks a reply must pass, and the client on two
//! stacks wired to each other, stepped a millisecond at a time.

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::time::Duration;

use assert2::{assert, check};
use ferrite_hal::{Instant, UnixTime};
use rstest::rstest;
use smoltcp::iface::SocketHandle;
use smoltcp::socket::udp;
use smoltcp::wire::{IpEndpoint, Ipv4Address, Ipv4Cidr};

use super::wire::{Kiss, Leap, Mode, PACKET_LEN, Packet, Stratum, Timestamp};
use super::*;
use crate::notify::{Host, HostName};
use crate::queue::QueuePort;
use crate::stack::{PollCtx, Stack, StackConfig};
use crate::tests::{State, Toy};
use crate::{Network, PortId};

/// A request to pool.ntp.org and what answered it, captured on the wire.
const REQUEST: &str =
    "230000000000000000000000000000000000000000000000000000000000000000000000000000004e7a360f78ae9c7b";
const REPLY: &str = "240203e800000557000004c14559cfc7eea29f6013a1cd584e7a360f78ae9c7beea2a10e0bca77fbeea2a10e0bce26cb";
const ORIGIN: Timestamp = Timestamp::new(0x4e7a_360f, 0x78ae_9c7b);
const REPLY_SERVER: Ipv4Address = Ipv4Address::new(216, 229, 0, 50);

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len()).step_by(2).map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap()).collect()
}

fn at(ms: u64) -> Instant {
    Instant::from_epoch(Duration::from_millis(ms))
}

fn utc(secs: u64) -> UnixTime {
    UnixTime::from_epoch(Duration::from_secs(secs))
}

/// When the captured reply was received.
const NOW: u64 = 1_794_646_670;

fn query() -> Query {
    Query { server: REPLY_SERVER, origin: ORIGIN, sent: at(10_000) }
}

#[test]
fn the_request_is_the_bytes_an_sntp_client_sends() {
    let request = Packet::request(ORIGIN);
    check!(request.encode().as_slice() == unhex(REQUEST).as_slice());
    check!(Packet::parse(&unhex(REQUEST)) == Ok(request));
}

#[test]
fn a_real_reply_is_read_field_by_field() {
    assert!(let Ok(reply) = Packet::parse(&unhex(REPLY)));
    check!(reply.leap == Leap::None);
    check!(reply.version == 4);
    check!(reply.mode == Mode::Server);
    check!(reply.stratum == Stratum::Server(2));
    check!(reply.poll == 3);
    check!(reply.precision == -24);
    check!(reply.reference_id == [0x45, 0x59, 0xcf, 0xc7]);
    check!(reply.origin == ORIGIN);
    check!(reply.receive == Timestamp::new(0xeea2_a10e, 0x0bca_77fb));
    check!(reply.transmit == Timestamp::new(0xeea2_a10e, 0x0bce_26cb));
    check!(reply.encode().as_slice() == unhex(REPLY).as_slice());
}

#[rstest]
#[case::empty(0)]
#[case::one_short(PACKET_LEN - 1)]
fn bytes_short_of_a_packet_are_refused(#[case] len: usize) {
    check!(Packet::parse(&vec![0; len]) == Err(wire::TooShort { len }));
}

#[test]
fn a_reply_with_a_mac_after_its_48_bytes_still_parses() {
    let mut bytes = unhex(REPLY);
    bytes.extend_from_slice(&[0xaa; 20]);
    check!(Packet::parse(&bytes) == Packet::parse(&unhex(REPLY)));
}

#[rstest]
#[case::now(0xeea2_a10e, NOW, NOW)]
#[case::the_last_second_before_the_wrap(0xffff_ffff, 2_085_978_000, 2_085_978_495)]
#[case::the_first_after_it_by_a_clock_before_it(0, 2_085_978_000, 2_085_978_496)]
#[case::the_first_after_it_by_a_clock_after_it(0, 2_085_979_000, 2_085_978_496)]
#[case::a_stale_clock_in_the_next_era(5, 2_085_978_000 + 3600 * 24 * 365 * 3, 2_085_978_496 + 5)]
fn the_era_of_a_timestamp_is_the_one_nearest_the_clock(#[case] seconds: u32, #[case] pivot: u64, #[case] want: u64) {
    let stamp = Timestamp::new(seconds, 0);
    check!(stamp.to_unix(utc(pivot)) == Some(utc(want)));
}

#[test]
fn a_time_before_1970_is_not_a_time() {
    check!(Timestamp::new(1_000_000_000, 0).to_unix(utc(0)) == None);
}

#[test]
fn the_fraction_is_nanoseconds_and_the_unix_seconds_go_back_to_ntp() {
    let half = Timestamp::new(0xeea2_a10e, 1 << 31).to_unix(utc(NOW)).unwrap();
    check!(half.since_epoch() == Duration::new(NOW, 500_000_000));
    check!(Timestamp::from_unix_seconds(utc(NOW)) == Timestamp::new(0xeea2_a10e, 0));
}

#[test]
fn the_real_reply_gives_the_servers_time_at_the_middle_of_the_exchange_and_the_delay() {
    let received = at(10_000 + 45);
    assert!(let Ok(sample) = query().accept(REPLY_SERVER, &unhex(REPLY), received, utc(NOW)));
    check!(sample.stratum == 2);
    check!(sample.at == at(10_022) + Duration::from_micros(500));
    // The server held the request 56 microseconds, and the round trip was 45 ms.
    check!(
        sample.delay > Duration::from_micros(44_900) && sample.delay < Duration::from_micros(44_950),
        "{:?}",
        sample.delay
    );
    let since = sample.utc.since_epoch();
    check!(since.as_secs() == NOW);
    check!((since.subsec_micros()).abs_diff(46_085) <= 1, "{since:?}");
}

fn reply_with(edit: impl FnOnce(&mut Packet)) -> Vec<u8> {
    let mut packet = Packet::parse(&unhex(REPLY)).unwrap();
    edit(&mut packet);
    packet.encode().to_vec()
}

#[rstest]
#[case::another_source(reply_with(|_| {}), Ipv4Address::new(10, 9, 9, 9), Reject::Source)]
#[case::someone_elses_request(reply_with(|p| p.origin = Timestamp::new(1, 2)), REPLY_SERVER, Reject::Origin)]
#[case::a_client_packet(reply_with(|p| p.mode = Mode::Client), REPLY_SERVER, Reject::NotAReply)]
#[case::a_broadcast(reply_with(|p| p.mode = Mode::Broadcast), REPLY_SERVER, Reject::NotAReply)]
#[case::version_two(reply_with(|p| p.version = 2), REPLY_SERVER, Reject::NotAReply)]
#[case::rate(reply_with(|p| { p.stratum = Stratum::Kiss; p.reference_id = *b"RATE"; }), REPLY_SERVER, Reject::Kiss(Kiss::Rate))]
#[case::an_odd_kiss(reply_with(|p| { p.stratum = Stratum::Kiss; p.reference_id = *b"XFAC"; }), REPLY_SERVER, Reject::Kiss(Kiss::Unknown(*b"XFAC")))]
#[case::alarm(reply_with(|p| p.leap = Leap::Alarm), REPLY_SERVER, Reject::Unsynchronized)]
#[case::stratum_16(reply_with(|p| p.stratum = Stratum::Unsynchronized), REPLY_SERVER, Reject::Unsynchronized)]
#[case::stratum_200(reply_with(|p| p.stratum = Stratum::Unknown(200)), REPLY_SERVER, Reject::Unsynchronized)]
#[case::no_transmit_time(reply_with(|p| p.transmit = Timestamp::ZERO), REPLY_SERVER, Reject::Timestamps)]
#[case::no_receive_time(reply_with(|p| p.receive = Timestamp::ZERO), REPLY_SERVER, Reject::Timestamps)]
#[case::transmitted_before_received(reply_with(|p| p.transmit = Timestamp::new(0xeea2_a10d, 0)), REPLY_SERVER, Reject::Timestamps)]
fn a_reply_that_is_not_to_be_believed_says_why(#[case] bytes: Vec<u8>, #[case] from: Ipv4Address, #[case] why: Reject) {
    check!(query().accept(from, &bytes, at(10_045), utc(NOW)) == Err(why));
}

#[test]
fn a_reply_that_took_over_a_second_is_too_blurred_to_set_a_clock_by() {
    let slow = query().accept(REPLY_SERVER, &unhex(REPLY), at(10_000 + 1_001), utc(NOW));
    check!(slow == Err(Reject::Delayed));
    check!(query().accept(REPLY_SERVER, &unhex(REPLY), at(10_000 + 1_000), utc(NOW)).is_ok());
}

#[test]
fn a_reply_dated_before_1970_by_the_clock_is_refused() {
    let bytes = reply_with(|p| {
        p.receive = Timestamp::new(1_000_000_000, 0);
        p.transmit = p.receive;
    });
    check!(query().accept(REPLY_SERVER, &bytes, at(10_045), utc(0)) == Err(Reject::Era));
}

#[test]
fn a_datagram_too_short_for_a_packet_is_a_stray() {
    let why = query().accept(REPLY_SERVER, &[0; 20], at(10_045), utc(NOW)).unwrap_err();
    check!(why == Reject::TooShort && why.is_stray());
    check!(Reject::Origin.is_stray() && !Reject::Kiss(Kiss::Rate).is_stray() && !Reject::Timestamps.is_stray());
}

#[rstest]
#[case(*b"RATE", true)]
#[case(*b"DENY", true)]
#[case(*b"RSTR", true)]
#[case(*b"INIT", false)]
fn only_the_kisses_that_say_go_away_ban_the_server(#[case] code: [u8; 4], #[case] bans: bool) {
    check!(Kiss::from_code(code).bans() == bans);
}

// The client's schedule, without sockets.

const SERVER_A: Ipv4Address = Ipv4Address::new(10, 0, 0, 1);
const SERVER_B: Ipv4Address = Ipv4Address::new(10, 0, 0, 2);
const LEASE: Ipv4Address = Ipv4Address::new(192, 0, 2, 1);
/// A request is given up on after two failures in a row; a round is four requests 1.5 s apart.
const SPACING: Duration = Duration::from_millis(1500);

fn entropy() -> Box<dyn FnMut() -> u64> {
    let mut n = 0_u64;
    Box::new(move || {
        n += 1;
        n.wrapping_mul(0x9e37_79b9_7f4a_7c15)
    })
}

fn addr(a: Ipv4Address) -> Host {
    Host::Addr(a)
}

fn name(n: &str) -> Host {
    Host::Name(HostName::new(n).unwrap())
}

/// The client driven the way a stack's sockets drive it, by hand.
struct Harness {
    sync: Sync,
    now: Instant,
}

impl Harness {
    fn new(servers: Vec<Host>) -> Self {
        Self { sync: Sync::new(servers, entropy(), utc(NOW)), now: at(1_000) }
    }

    fn step(&mut self, by: Duration) {
        self.now = self.now + by;
    }

    fn ask(&mut self) -> Option<Request> {
        self.sync.next_request(self.now, &[])
    }

    /// Send the request in flight to `server` and hear its reply `rtt` later, from a server whose clock
    /// reads the second after `NOW` plus `rtt`.
    fn reply(&mut self, server: Ipv4Address, rtt: Duration) -> Heard {
        let query = self.sync.begin(server, self.now).unwrap();
        self.step(rtt);
        let mut packet = Packet::parse(&unhex(REPLY)).unwrap();
        packet.origin = query.origin;
        packet.receive = Timestamp::new(0xeea2_a10e + 1, 0);
        packet.transmit = packet.receive;
        self.sync.heard(server, &packet.encode(), self.now)
    }

    /// Fail every request until the round is over; how long until the next round is due.
    fn lose_a_round(&mut self) -> Duration {
        for _ in 0..40 {
            if self.ask().is_some() {
                self.sync.failed(Failure::Timeout, self.now);
            }
            if !self.sync.wants_port(self.now) {
                break;
            }
            self.step(SPACING);
        }
        let over = self.now;
        while !self.sync.wants_port(self.now) {
            self.step(Duration::from_secs(1));
        }
        self.now - over
    }

    /// Take four replies of the given round trips from `server`, 1.5 s apart.
    fn sync_with(&mut self, server: Ipv4Address, rtts_ms: [u64; 4]) {
        for rtt in rtts_ms {
            while self.ask().is_none() {
                self.step(Duration::from_millis(100));
            }
            check!(self.reply(server, Duration::from_millis(rtt)) == Heard::Settled);
        }
    }
}

#[test]
fn the_client_asks_the_settings_first_then_the_lease_then_the_pool_and_each_once() {
    let mut h = Harness::new(vec![addr(SERVER_A), name("time.lab"), addr(SERVER_A)]);
    let mut asked = Vec::new();
    for _ in 0..40 {
        if let Some(request) = h.sync.next_request(h.now, &[LEASE, SERVER_A]) {
            if asked.last() != Some(&request.host) {
                asked.push(request.host);
            }
            h.sync.failed(Failure::Timeout, h.now);
        }
        h.step(SPACING);
    }
    check!(asked == [addr(SERVER_A), name("time.lab"), addr(LEASE), name("pool.ntp.org")]);
}

#[test]
fn four_samples_are_taken_and_the_one_with_the_least_delay_is_the_fix() {
    let mut h = Harness::new(vec![addr(SERVER_A)]);
    h.sync_with(SERVER_A, [80, 30, 50, 120]);

    assert!(let Some(fix) = h.sync.take_fix());
    check!(fix.samples == 4);
    check!(fix.sample.server == SERVER_A);
    check!(fix.sample.delay == Duration::from_millis(30));
    check!(h.sync.take_fix().is_none(), "a fix comes out once");
    check!(!h.sync.wants_port(h.now + Duration::from_mins(29)));
    check!(h.sync.wants_port(h.now + Duration::from_mins(30)));
}

#[test]
fn a_datagram_that_is_not_the_reply_is_dropped_and_the_wait_goes_on() {
    let mut h = Harness::new(vec![addr(SERVER_A)]);
    assert!(let Some(_) = h.ask());
    let query = h.sync.begin(SERVER_A, h.now).unwrap();
    check!(h.sync.heard(SERVER_B, &unhex(REPLY), h.now) == Heard::Stray);
    check!(h.sync.heard(SERVER_A, &[0; 10], h.now) == Heard::Stray);
    let mut packet = Packet::parse(&unhex(REPLY)).unwrap();
    packet.origin = Timestamp::new(9, 9);
    check!(h.sync.heard(SERVER_A, &packet.encode(), h.now) == Heard::Stray);
    check!(h.ask().is_none(), "the request is still in flight");
    packet.origin = query.origin;
    h.step(Duration::from_millis(20));
    check!(h.sync.heard(SERVER_A, &packet.encode(), h.now) == Heard::Settled);
}

#[test]
fn a_server_that_sends_a_kiss_of_death_is_never_asked_again() {
    let mut h = Harness::new(vec![addr(SERVER_A), addr(SERVER_B)]);
    assert!(let Some(first) = h.ask());
    check!(first.host == addr(SERVER_A));
    let query = h.sync.begin(SERVER_A, h.now).unwrap();
    let mut kiss = Packet::parse(&unhex(REPLY)).unwrap();
    kiss.origin = query.origin;
    kiss.stratum = Stratum::Kiss;
    kiss.reference_id = *b"RATE";
    check!(h.sync.heard(SERVER_A, &kiss.encode(), h.now) == Heard::Settled);

    assert!(let Some(second) = h.ask());
    check!(second.host == addr(SERVER_B), "the next server is tried at once");
    h.sync.failed(Failure::Timeout, h.now);
    h.step(SPACING);
    assert!(let Some(_) = h.ask());
    h.sync.failed(Failure::Timeout, h.now);
    // The pool is what is left; its answer is from the banned server's address.
    assert!(let Some(pool) = h.ask());
    check!(pool.host == name("pool.ntp.org"));
    check!(h.sync.begin(SERVER_A, h.now).is_none(), "a banned address is not sent to");
}

#[test]
fn a_kiss_that_is_not_a_ban_only_fails_the_request() {
    let mut h = Harness::new(vec![addr(SERVER_A)]);
    for _ in 0..2 {
        assert!(let Some(_) = h.ask());
        let query = h.sync.begin(SERVER_A, h.now).unwrap();
        let mut kiss = Packet::parse(&unhex(REPLY)).unwrap();
        kiss.origin = query.origin;
        kiss.stratum = Stratum::Kiss;
        kiss.reference_id = *b"INIT";
        check!(h.sync.heard(SERVER_A, &kiss.encode(), h.now) == Heard::Settled);
        h.step(SPACING);
    }
    check!(h.sync.begin(SERVER_A, h.now).is_none());
    assert!(let Some(request) = h.ask(), "the pool is next, not the end");
    check!(request.host == name("pool.ntp.org"));
}

#[test]
fn a_failed_round_waits_a_minute_and_each_failure_after_doubles_it_up_to_half_an_hour() {
    let mut h = Harness::new(vec![]);
    let waits: Vec<Duration> = (0..7).map(|_| h.lose_a_round()).collect();
    let minutes: Vec<u64> = waits.iter().map(|wait| wait.as_secs() / 60).collect();
    check!(minutes == [1, 2, 4, 8, 16, 30, 30]);

    // A sync starts the schedule over.
    h.sync_with(SERVER_A, [10; 4]);
    check!(h.sync.take_fix().is_some());
    h.step(Duration::from_mins(30));
    let wait = h.lose_a_round();
    check!(wait == Duration::from_mins(1));
}

#[test]
fn a_warning_is_due_two_minutes_after_an_address_and_again_only_after_a_sync_has_failed() {
    let mut h = Harness::new(vec![addr(SERVER_A)]);
    h.sync.observe(h.now, true);
    h.step(Duration::from_secs(119));
    h.sync.observe(h.now, true);
    check!(!h.sync.warned());
    h.step(Duration::from_secs(1));
    h.sync.observe(h.now, true);
    check!(h.sync.warned());

    h.sync_with(SERVER_A, [10; 4]);
    check!(!h.sync.warned(), "a sync ends the streak");
    h.step(Duration::from_mins(30));
    h.sync.observe(h.now, true);
    check!(!h.sync.warned(), "waiting on the next sync is not a failure");
    h.lose_a_round();
    check!(h.sync.warned());
}

#[test]
fn losing_the_address_restarts_the_two_minutes() {
    let mut h = Harness::new(vec![]);
    h.sync.observe(h.now, true);
    h.step(Duration::from_secs(100));
    h.sync.observe(h.now, false);
    h.sync.observe(h.now, true);
    h.step(Duration::from_secs(100));
    h.sync.observe(h.now, true);
    check!(!h.sync.warned());
}

// The client on two stacks.

const SERVER: Ipv4Address = Ipv4Address::new(10, 0, 0, 1);
const CLIENT: Ipv4Address = Ipv4Address::new(10, 0, 0, 2);

fn cidr(address: Ipv4Address) -> Ipv4Cidr {
    Ipv4Cidr::new(address, 24)
}

/// What the server does with a request.
#[derive(Clone, Copy)]
enum Answer {
    /// Reply with its clock.
    Reply,
    Kiss([u8; 4]),
    Silent,
}

struct Rig {
    net: Network<Toy>,
    port: PortId,
    server: Stack,
    server_link: Rc<QueuePort>,
    client_link: Rc<QueuePort>,
    ntp: SocketHandle,
    dns: SocketHandle,
    answer: Answer,
    /// Requests the server read, and the sources they came from.
    requests: Vec<(Packet, IpEndpoint)>,
    lookups: Vec<String>,
    base_sockets: usize,
    ms: u64,
}

impl Rig {
    fn new(servers: Vec<Host>) -> Self {
        let server_link = Rc::new(QueuePort::new([0x02, 0, 0, 0, 0, 1], 1500, 64, 64));
        let client_link = Rc::new(QueuePort::new([0x02, 0, 0, 0, 0, 2], 1500, 64, 64));
        let mut server = Stack::new(
            Box::new(Rc::clone(&server_link)),
            StackConfig { seed: 1, address: Some(cidr(SERVER)), mdns: false, http: false },
            Instant::default(),
        );
        let (_, sockets) = server.parts();
        let mut bound = |port: u16| {
            let buffer = || udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 8], vec![0; 2048]);
            let mut socket = udp::Socket::new(buffer(), buffer());
            socket.bind(port).unwrap();
            sockets.add(socket)
        };
        let (ntp, dns) = (bound(wire::PORT), bound(53));

        let mut net = Network::new(Toy::default());
        let config = StackConfig { seed: 2, address: Some(cidr(CLIENT)), mdns: false, http: false };
        let port = net.add(Box::new(Rc::clone(&client_link)), config, Instant::default());
        net.stack_mut(port).unwrap().set_dns(Some(SERVER));
        net.set_sync(Sync::new(servers, entropy(), utc(NOW)));
        let base_sockets = net.stack_mut(port).unwrap().parts().1.iter().count();
        Self {
            net,
            port,
            server,
            server_link,
            client_link,
            ntp,
            dns,
            answer: Answer::Reply,
            requests: Vec::new(),
            lookups: Vec::new(),
            base_sockets,
            ms: 0,
        }
    }

    /// The server's clock at the simulated millisecond `ms`, as an NTP timestamp: the pivot's second plus
    /// the time since the start.
    fn server_time(ms: u64) -> Timestamp {
        let since = Duration::from_millis(ms);
        let unix = Duration::from_secs(NOW) + since;
        let seconds = (unix.as_secs() + 2_208_988_800) as u32;
        Timestamp::new(seconds, ((u64::from(unix.subsec_nanos()) << 32) / 1_000_000_000) as u32)
    }

    fn step(&mut self) {
        self.ms += 1;
        let now = at(self.ms);
        self.net.set_time_pivot(utc(NOW));
        self.net.poll(now, &State { revision: 0 }, &mut |_| {});
        let mut render = |_: &mut String| {};
        let mut render_text = |_: &mut String| {};
        self.server.poll(
            now,
            &mut PollCtx { render: &mut render, render_text: &mut render_text, clock: None, notify: None, sntp: None },
        );
        self.serve_ntp();
        self.serve_dns();
        for (from, to) in [(&self.client_link, &self.server_link), (&self.server_link, &self.client_link)] {
            while let Some(frame) = from.next_tx() {
                from.transmitted(true);
                to.deliver(&frame);
            }
        }
    }

    fn run(&mut self, ms: u64) {
        (0..ms).for_each(|_| self.step());
    }

    fn run_until_fix(&mut self, limit: u64) -> Option<Fix> {
        for _ in 0..limit {
            self.step();
            if let Some(fix) = self.net.take_time_fix() {
                return Some(fix);
            }
        }
        None
    }

    fn serve_ntp(&mut self) {
        let sock = self.server.parts().1.get_mut::<udp::Socket<'_>>(self.ntp);
        while let Ok((bytes, meta)) = sock.recv() {
            let request = Packet::parse(bytes).unwrap();
            self.requests.push((request, meta.endpoint));
            let mut reply = Packet::request(Timestamp::ZERO);
            reply.mode = Mode::Server;
            reply.stratum = Stratum::Server(2);
            reply.origin = request.transmit;
            match self.answer {
                Answer::Silent => continue,
                Answer::Reply => {
                    reply.receive = Self::server_time(self.ms);
                    reply.transmit = Self::server_time(self.ms);
                },
                Answer::Kiss(code) => {
                    reply.stratum = Stratum::Kiss;
                    reply.reference_id = code;
                },
            }
            sock.send_slice(&reply.encode(), meta.endpoint).unwrap();
        }
    }

    fn serve_dns(&mut self) {
        let sock = self.server.parts().1.get_mut::<udp::Socket<'_>>(self.dns);
        while let Ok((query, meta)) = sock.recv() {
            let mut at = 12;
            let mut parts = Vec::new();
            while query[at] != 0 {
                let len = usize::from(query[at]);
                parts.push(String::from_utf8_lossy(&query[at + 1..at + 1 + len]).into_owned());
                at += 1 + len;
            }
            let question_end = at + 1 + 4;
            self.lookups.push(parts.join("."));
            let mut reply = vec![query[0], query[1], 0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 0];
            reply.extend_from_slice(&query[12..question_end]);
            reply.extend_from_slice(&[0xC0, 0x0C, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4]);
            reply.extend_from_slice(&SERVER.octets());
            sock.send_slice(&reply, meta.endpoint).unwrap();
        }
    }

    fn client_sockets(&mut self) -> usize {
        self.net.stack_mut(self.port).unwrap().parts().1.iter().count()
    }
}

#[test]
fn a_server_given_by_address_is_asked_four_times_and_the_time_is_the_servers_at_the_middle_of_the_exchange() {
    let mut rig = Rig::new(vec![addr(SERVER)]);
    assert!(let Some(fix) = rig.run_until_fix(12_000));

    check!(rig.requests.len() == 4);
    check!(rig.lookups.is_empty(), "an address needs no lookup");
    check!(fix.samples == 4 && fix.sample.server == SERVER && fix.sample.stratum == 2);
    check!(rig.requests.iter().all(|(request, _)| request.mode == Mode::Client && request.version == 4));
    let origins: Vec<_> = rig.requests.iter().map(|(request, _)| request.transmit).collect();
    check!(
        origins.iter().enumerate().all(|(i, o)| !o.is_zero() && !origins[..i].contains(o)),
        "each request has its own"
    );
    let ports: Vec<_> = rig.requests.iter().map(|(_, from)| from.port).collect();
    check!(ports.iter().enumerate().all(|(i, p)| *p >= 49_152 && !ports[..i].contains(p)));

    // The server's clock is the simulated time on top of `NOW`: the fix says as much, within a poll.
    let truth = Duration::from_secs(NOW) + fix.sample.at.since_epoch();
    let error = fix.sample.utc.since_epoch().abs_diff(truth);
    check!(error <= Duration::from_millis(3), "{error:?}");
    check!(fix.sample.delay <= Duration::from_millis(10), "{:?}", fix.sample.delay);
    check!(rig.client_sockets() == rig.base_sockets, "the request's socket is given back");
}

#[test]
fn a_host_name_is_looked_up_through_the_lease_dns_server_and_the_pool_is_the_last_resort() {
    let mut rig = Rig::new(vec![name("time.lab")]);
    check!(rig.run_until_fix(12_000).is_some());
    check!(rig.lookups.first().map(String::as_str) == Some("time.lab"));
    check!(rig.requests.len() == 4);

    let mut pool = Rig::new(vec![]);
    check!(pool.run_until_fix(12_000).is_some());
    check!(pool.lookups.first().map(String::as_str) == Some("pool.ntp.org"));
}

#[test]
fn a_server_that_sends_rate_gets_one_request_and_is_not_used_again_even_under_another_name() {
    let mut rig = Rig::new(vec![addr(SERVER)]);
    rig.answer = Answer::Kiss(*b"RATE");
    check!(rig.run_until_fix(30_000).is_none());
    check!(rig.requests.len() == 1, "the pool resolves to the banned address and is not sent to");
    check!(rig.client_sockets() == rig.base_sockets);
}

#[test]
fn a_silent_server_costs_two_requests_a_host_and_leaves_no_sockets() {
    let mut rig = Rig::new(vec![addr(SERVER)]);
    rig.answer = Answer::Silent;
    check!(rig.run_until_fix(20_000).is_none());
    // The address, then the pool name that resolves to the same silent server.
    check!(rig.requests.len() == 4);
    check!(rig.client_sockets() == rig.base_sockets);
}

#[test]
fn the_first_port_with_an_address_carries_the_requests() {
    let mut rig = Rig::new(vec![addr(SERVER)]);
    let mut net = Network::new(Toy::default());
    let none = StackConfig { seed: 3, address: None, mdns: false, http: false };
    let first =
        net.add(Box::new(Rc::new(QueuePort::new([0x02, 0, 0, 0, 0, 3], 1500, 64, 64))), none, Instant::default());
    let fixed = StackConfig { seed: 2, address: Some(cidr(CLIENT)), mdns: false, http: false };
    rig.port = net.add(Box::new(Rc::clone(&rig.client_link)), fixed, Instant::default());
    net.set_sync(Sync::new(vec![addr(SERVER)], entropy(), utc(NOW)));
    rig.net = net;

    check!(rig.run_until_fix(12_000).is_some());
    check!(rig.net.stack(first).unwrap().link().address.is_none());
}

#[test]
fn a_stack_removed_in_the_middle_of_a_request_does_not_strand_the_client() {
    let mut rig = Rig::new(vec![addr(SERVER)]);
    rig.answer = Answer::Silent;
    rig.run(100);
    check!(rig.net.stack(rig.port).unwrap().syncing());
    rig.net.remove(rig.port);

    let config = StackConfig { seed: 5, address: Some(cidr(CLIENT)), mdns: false, http: false };
    rig.port = rig.net.add(Box::new(Rc::clone(&rig.client_link)), config, Instant::default());
    rig.net.stack_mut(rig.port).unwrap().set_dns(Some(SERVER));
    rig.answer = Answer::Reply;
    check!(rig.run_until_fix(12_000).is_some());
}
