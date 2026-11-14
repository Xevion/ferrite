//! A client network and a tiny HTTP and DNS server on two stacks wired to each other through
//! [`QueuePort`]s, stepped a millisecond at a time.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::time::Duration;

use assert2::{assert, check};
use ferrite_hal::Instant;
use rstest::rstest;
use smoltcp::iface::SocketHandle;
use smoltcp::socket::{tcp, udp};
use smoltcp::wire::{Ipv4Address, Ipv4Cidr};

use super::*;
use crate::queue::QueuePort;
use crate::stack::{PollCtx, Stack, StackConfig};
use crate::tests::{State, Toy};
use crate::{Network, PortId};

const SERVER: Ipv4Address = Ipv4Address::new(10, 0, 0, 1);
const CLIENT: Ipv4Address = Ipv4Address::new(10, 0, 0, 2);
const HTTP_PORT: u16 = 8080;

fn cidr(address: Ipv4Address) -> Ipv4Cidr {
    Ipv4Cidr::new(address, 24)
}

/// A policy with waits short enough to run through in a few simulated seconds.
fn quick() -> Policy {
    Policy {
        capacity: 8,
        attempt_timeout: Duration::from_millis(600),
        retry_base: Duration::from_millis(100),
        retry_max: Duration::from_millis(400),
        max_attempts: 3,
    }
}

struct Rig {
    net: Network<Toy>,
    port: PortId,
    server: Stack,
    server_link: Rc<QueuePort>,
    client_link: Rc<QueuePort>,
    http: SocketHandle,
    dns: SocketHandle,
    /// What the server answers each request with, in turn; `None` stays silent. Then 200.
    script: VecDeque<Option<u16>>,
    inbox: Vec<u8>,
    /// Complete requests the server read.
    requests: Vec<String>,
    /// Names the DNS server was asked for.
    lookups: Vec<String>,
    /// Sockets the client stack had before any notification.
    base_sockets: usize,
    ms: u64,
}

impl Rig {
    fn new(env: &str, policy: Policy) -> Self {
        let server_link = Rc::new(QueuePort::new([0x02, 0, 0, 0, 0, 1], 1500, 64, 64));
        let client_link = Rc::new(QueuePort::new([0x02, 0, 0, 0, 0, 2], 1500, 64, 64));
        let mut server = Stack::new(
            Box::new(Rc::clone(&server_link)),
            StackConfig { seed: 1, address: Some(cidr(SERVER)), mdns: false, http: false },
            Instant::default(),
        );
        let (_, sockets) = server.parts();
        let http =
            sockets.add(tcp::Socket::new(tcp::SocketBuffer::new(vec![0; 4096]), tcp::SocketBuffer::new(vec![0; 1024])));
        let dns = sockets.add({
            let buffer = || udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0; 1024]);
            let mut socket = udp::Socket::new(buffer(), buffer());
            socket.bind(53).unwrap();
            socket
        });

        let mut net = Network::new(Toy::default());
        let config = StackConfig { seed: 2, address: Some(cidr(CLIENT)), mdns: false, http: false };
        let port = net.add(Box::new(Rc::clone(&client_link)), config, Instant::default());
        net.stack_mut(port).unwrap().set_dns(Some(SERVER));
        net.set_notifier(Notifier::with_policy(Config::from_env(env.as_bytes()).unwrap(), policy));
        let base_sockets = net.stack_mut(port).unwrap().parts().1.iter().count();
        Self {
            net,
            port,
            server,
            server_link,
            client_link,
            http,
            dns,
            script: VecDeque::new(),
            inbox: Vec::new(),
            requests: Vec::new(),
            lookups: Vec::new(),
            base_sockets,
            ms: 0,
        }
    }

    fn step(&mut self) {
        self.ms += 1;
        let now = Instant::from_epoch(Duration::from_millis(self.ms));
        self.net.poll(now, &State { revision: 0 }, &mut |_| {});
        let mut render = |_: &mut String| {};
        self.server.poll(now, &mut PollCtx { render: &mut render, clock: None, notify: None });
        self.serve_http();
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

    fn run_until(&mut self, limit: u64, mut done: impl FnMut(&Self) -> bool) {
        for _ in 0..limit {
            if done(self) {
                return;
            }
            self.step();
        }
    }

    fn serve_http(&mut self) {
        let sock = self.server.parts().1.get_mut::<tcp::Socket<'_>>(self.http);
        match sock.state() {
            tcp::State::TimeWait => sock.abort(),
            tcp::State::Closed => {
                sock.listen(HTTP_PORT).unwrap();
                self.inbox.clear();
            },
            tcp::State::CloseWait => sock.close(),
            _ => {},
        }
        while sock.can_recv() {
            let inbox = &mut self.inbox;
            sock.recv(|buf| {
                inbox.extend_from_slice(buf);
                (buf.len(), ())
            })
            .unwrap();
        }
        let text = String::from_utf8_lossy(&self.inbox).into_owned();
        let Some((head, body)) = text.split_once("\r\n\r\n") else { return };
        let length = head
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .and_then(|n| n.parse::<usize>().ok())
            .unwrap_or(0);
        if body.len() < length || !sock.may_send() {
            return;
        }
        self.requests.push(text.clone());
        self.inbox.clear();
        if let Some(status) = self.script.pop_front().unwrap_or(Some(200)) {
            let reply = format!("HTTP/1.1 {status} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            sock.send_slice(reply.as_bytes()).unwrap();
            sock.close();
        }
    }

    fn serve_dns(&mut self) {
        let sock = self.server.parts().1.get_mut::<udp::Socket<'_>>(self.dns);
        while let Ok((query, meta)) = sock.recv() {
            let mut at = 12;
            let mut name = Vec::new();
            while query[at] != 0 {
                let len = usize::from(query[at]);
                name.push(String::from_utf8_lossy(&query[at + 1..at + 1 + len]).into_owned());
                at += 1 + len;
            }
            let question_end = at + 1 + 4;
            self.lookups.push(name.join("."));
            let mut reply = vec![query[0], query[1], 0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 0];
            reply.extend_from_slice(&query[12..question_end]);
            reply.extend_from_slice(&[0xC0, 0x0C, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4]);
            reply.extend_from_slice(&SERVER.octets());
            sock.send_slice(&reply, meta.endpoint).unwrap();
        }
    }

    fn delivered(&self) -> u64 {
        self.net.notification_stats().unwrap().delivered
    }

    fn client_sockets(&mut self) -> usize {
        self.net.stack_mut(self.port).unwrap().parts().1.iter().count()
    }
}

fn pass(n: usize) -> Notification {
    Notification::new(Event::Pass, format!("ferrite pass {n}"), format!("ferrite: pass {n} done"))
}

#[test]
fn a_text_notification_arrives_as_a_post_to_an_address() {
    let mut rig = Rig::new("NOTIFY_URL=http://10.0.0.1:8080/ferrite-topic", Policy::default());
    check!(rig.net.notify(pass(3)));
    rig.run_until(2_000, |rig| rig.delivered() == 1);

    check!(rig.delivered() == 1);
    check!(rig.net.notifications_pending() == 0);
    check!(
        rig.requests
            == ["POST /ferrite-topic HTTP/1.1\r\nHost: 10.0.0.1:8080\r\nContent-Type: text/plain; charset=utf-8\r\n\
                 Content-Length: 20\r\nConnection: close\r\nTitle: ferrite pass 3\r\n\r\nferrite: pass 3 done"]
    );
    check!(rig.lookups.is_empty(), "an address needs no lookup");
}

#[test]
fn a_json_notification_to_a_host_name_is_looked_up_first() {
    let mut rig = Rig::new("NOTIFY_URL=http://hooks.lab:8080/in\nNOTIFY_FORMAT=json", Policy::default());
    check!(rig.net.notify(pass(1)));
    rig.run_until(3_000, |rig| rig.delivered() == 1);

    check!(rig.lookups == ["hooks.lab"]);
    assert!(let [request] = rig.requests.as_slice());
    check!(request.starts_with("POST /in HTTP/1.1\r\nHost: hooks.lab:8080\r\nContent-Type: application/json\r\n"));
    check!(request.ends_with(r#"{"event":"pass","title":"ferrite pass 1","message":"ferrite: pass 1 done"}"#));
}

#[test]
fn events_the_config_leaves_out_are_not_queued() {
    let mut rig = Rig::new("NOTIFY_URL=http://10.0.0.1:8080/t\nNOTIFY_EVENTS=fault,end", Policy::default());
    check!(!rig.net.notify(pass(1)));
    check!(rig.net.notifications_pending() == 0);
    check!(rig.net.notify(Notification::new(Event::End, "t", "m")));
    check!(rig.net.notifications_pending() == 1);
}

#[test]
fn a_busy_server_is_retried_and_the_order_kept() {
    let mut rig = Rig::new("NOTIFY_URL=http://10.0.0.1:8080/t", quick());
    rig.script.extend([Some(503), Some(200), Some(200)]);
    check!(rig.net.notify(pass(1)));
    check!(rig.net.notify(pass(2)));
    rig.run_until(5_000, |rig| rig.delivered() == 2);

    check!(rig.delivered() == 2);
    let bodies: Vec<_> = rig.requests.iter().map(|r| String::from(r.rsplit("\r\n\r\n").next().unwrap())).collect();
    check!(bodies == ["ferrite: pass 1 done", "ferrite: pass 1 done", "ferrite: pass 2 done"]);
}

#[rstest]
#[case::a_refusal_is_not_retried("http://10.0.0.1:8080/t", &[Some(404)], 1_500, 1)]
#[case::a_server_that_never_answers_is_tried_three_times("http://10.0.0.1:8080/t", &[None, None, None], 8_000, 3)]
#[case::nothing_listening_is_a_failed_attempt_not_a_hang("http://10.0.0.1:9/t", &[], 4_000, 0)]
fn a_failing_server_is_given_up_on_and_leaves_no_sockets(
    #[case] url: &str,
    #[case] script: &[Option<u16>],
    #[case] ms: u64,
    #[case] requests: usize,
) {
    let mut rig = Rig::new(&format!("NOTIFY_URL={url}"), quick());
    rig.script.extend(script.iter().copied());
    check!(rig.net.notify(pass(1)));
    rig.run(ms);
    check!(rig.net.notification_stats() == Some(Stats { delivered: 0, dropped: 0, abandoned: 1 }));
    check!(rig.net.notifications_pending() == 0);
    check!(rig.requests.len() == requests);
    check!(rig.client_sockets() == rig.base_sockets);
}

#[test]
fn a_full_queue_drops_the_oldest_while_nothing_can_be_sent() {
    let mut rig = Rig::new("NOTIFY_URL=http://10.0.0.1:8080/t", Policy { capacity: 3, ..quick() });
    // Nothing runs, so nothing leaves.
    (1..=5).for_each(|n| {
        rig.net.notify(pass(n));
    });
    check!(rig.net.notifications_pending() == 3);
    check!(rig.net.notification_stats().unwrap().dropped == 2);
    rig.run_until(3_000, |rig| rig.delivered() == 3);
    let bodies: Vec<_> = rig.requests.iter().map(|r| String::from(r.rsplit("\r\n\r\n").next().unwrap())).collect();
    check!(bodies == ["ferrite: pass 3 done", "ferrite: pass 4 done", "ferrite: pass 5 done"]);
}

#[test]
fn the_first_port_with_an_address_carries_it() {
    let mut rig = Rig::new("NOTIFY_URL=http://10.0.0.1:8080/t", quick());
    // A port ahead of the wired one that never gets an address.
    let mut net = Network::new(Toy::default());
    let none = StackConfig { seed: 3, address: None, mdns: false, http: false };
    let first =
        net.add(Box::new(Rc::new(QueuePort::new([0x02, 0, 0, 0, 0, 3], 1500, 64, 64))), none, Instant::default());
    let fixed = StackConfig { seed: 2, address: Some(cidr(CLIENT)), mdns: false, http: false };
    rig.port = net.add(Box::new(Rc::clone(&rig.client_link)), fixed, Instant::default());
    net.set_notifier(Notifier::with_policy(Config::from_env(b"NOTIFY_URL=http://10.0.0.1:8080/t").unwrap(), quick()));
    rig.net = net;

    check!(rig.net.notify(pass(1)));
    rig.run_until(2_000, |rig| rig.delivered() == 1);
    check!(rig.delivered() == 1);
    check!(rig.net.stack(first).unwrap().link().address.is_none());
}
