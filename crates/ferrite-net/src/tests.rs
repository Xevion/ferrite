//! A network of loopback stacks serving a toy site, with a client socket on each stack, stepped a
//! millisecond at a time.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::Cell;
use core::fmt::{self, Write as _};
use core::time::Duration;

use assert2::check;
use ferrite_hal::Instant;
use rstest::rstest;
use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp;
use smoltcp::wire::{IpAddress, Ipv4Address, Ipv4Cidr};

use crate::http::{Rejection, Route, Site};
use crate::loopback::LoopbackPort;
use crate::port::MacAddr;
use crate::status::NetStatus;
use crate::testing::count;
use crate::{Network, PortId, StackConfig};

const HOST: &str = "Host: ferrite.local\r\n";
const PAGE: &str = "<!doctype html><title>toy</title>";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    Pause,
    Echo,
}

impl From<Control> for &'static str {
    fn from(control: Control) -> Self {
        match control {
            Control::Pause => "pause",
            Control::Echo => "echo",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Pause,
    Echo(u32),
    Quit,
}

#[derive(Debug)]
pub struct NotANumber;

impl fmt::Display for NotANumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not a number")
    }
}

/// The app's state as one poll sees it.
pub struct State {
    pub revision: u64,
}

#[derive(Default)]
pub struct Toy {
    renders: Cell<u32>,
}

impl Site for Toy {
    type State<'a> = State;
    type Control = Control;
    type Command = Command;
    type BodyError = NotANumber;
    type Key = u64;

    fn page(&self) -> &'static str {
        PAGE
    }

    fn route(&self, method: &str, path: &str) -> Result<Route<Control>, Rejection> {
        let (route, allow) = match path {
            "/" => (Route::Page, "GET"),
            "/doc" => (Route::Document, "GET"),
            "/events" => (Route::Events, "GET"),
            "/text" => (Route::Text, "GET"),
            "/pause" => (Route::Control(Control::Pause), "POST"),
            "/echo" | "/quit" => (Route::Control(Control::Echo), "POST"),
            _ => return Err(Rejection::NotFound),
        };
        if method == allow { Ok(route) } else { Err(Rejection::MethodNotAllowed { allow }) }
    }

    fn command(&self, control: Control, body: &[u8]) -> Result<Command, NotANumber> {
        match control {
            Control::Pause => Ok(Command::Pause),
            Control::Echo => {
                let n = core::str::from_utf8(body).ok().and_then(|s| s.trim().parse().ok()).ok_or(NotANumber)?;
                Ok(if n == 0 { Command::Quit } else { Command::Echo(n) })
            },
        }
    }

    fn ends_run(command: &Command) -> bool {
        *command == Command::Quit
    }

    fn key(&self, state: &State) -> u64 {
        state.revision
    }

    fn render_text(&self, state: &State, _net: &NetStatus, _now: Instant, out: &mut String) {
        let _ = write!(out, "report of revision {}", state.revision);
    }

    fn render(&self, state: &State, net: &NetStatus, now: Instant, out: &mut String) {
        self.renders.set(self.renders.get() + 1);
        let _ = write!(
            out,
            "{{\"uptime_ms\":{},\"revision\":{},\"ports\":{}}}",
            now.since_epoch().as_millis(),
            state.revision,
            net.ports().len()
        );
    }
}

struct Rig {
    net: Network<Toy>,
    ids: Vec<PortId>,
    clients: Vec<SocketHandle>,
    state: State,
    ms: u64,
    commands: Vec<Command>,
    reply: Vec<u8>,
    allocs: u64,
}

fn cidr(last: u8) -> Ipv4Cidr {
    Ipv4Cidr::new(Ipv4Address::new(127, 0, 0, last), 8)
}

impl Rig {
    /// One stack per entry, on loopback addresses 127.0.0.`last`.
    fn new(lasts: &[u8]) -> Self {
        let mut net = Network::new(Toy::default());
        let mut ids = Vec::new();
        let mut clients = Vec::new();
        for &last in lasts {
            let port = LoopbackPort::with_mac(MacAddr::new([0x02, 0, 0, 0, 0, last]));
            let config = StackConfig { seed: 1, address: Some(cidr(last)), mdns: false, http: true };
            let id = net.add(Box::new(port), config, Instant::default());
            let (_, sockets) = net.stack_mut(id).unwrap().parts();
            clients.push(
                sockets.add(tcp::Socket::new(
                    tcp::SocketBuffer::new(vec![0; 65536]),
                    tcp::SocketBuffer::new(vec![0; 4096]),
                )),
            );
            ids.push(id);
        }
        Self {
            net,
            ids,
            clients,
            state: State { revision: 0 },
            ms: 0,
            commands: Vec::with_capacity(64),
            reply: Vec::new(),
            allocs: 0,
        }
    }

    fn client(&mut self, at: usize) -> &mut tcp::Socket<'static> {
        let (id, handle) = (self.ids[at], self.clients[at]);
        self.net.stack_mut(id).unwrap().parts().1.get_mut(handle)
    }

    fn connect(&mut self, at: usize, local_port: u16) {
        self.reply.clear();
        let (id, handle) = (self.ids[at], self.clients[at]);
        let last = self.net.stack(id).unwrap().link().address.unwrap().address();
        let (iface, sockets) = self.net.stack_mut(id).unwrap().parts();
        sockets
            .get_mut::<tcp::Socket<'_>>(handle)
            .connect(iface.context(), (IpAddress::Ipv4(last), 80), local_port)
            .unwrap();
        self.run_until(at, tcp::Socket::may_send, 100);
    }

    fn run_until(&mut self, at: usize, mut done: impl FnMut(&tcp::Socket<'static>) -> bool, limit: u64) {
        for _ in 0..limit {
            self.ms += 1;
            let now = Instant::from_epoch(Duration::from_millis(self.ms));
            let (net, state, commands) = (&mut self.net, &self.state, &mut self.commands);
            let (_, allocs) = count(|| net.poll(now, state, &mut |c| commands.push(c)));
            self.allocs += allocs;
            let (id, handle) = (self.ids[at], self.clients[at]);
            let reply = &mut self.reply;
            let sock = self.net.stack_mut(id).unwrap().parts().1.get_mut::<tcp::Socket<'static>>(handle);
            if sock.can_recv() {
                sock.recv(|buf| {
                    reply.extend_from_slice(buf);
                    (buf.len(), ())
                })
                .unwrap();
            }
            if done(sock) {
                return;
            }
        }
    }

    fn run(&mut self, ms: u64) {
        self.run_until(0, |_| false, ms);
    }

    fn request(&mut self, at: usize, request: &str) {
        check!(self.client(at).send_slice(request.as_bytes()).unwrap() == request.len());
    }

    fn reply(&self) -> String {
        String::from_utf8_lossy(&self.reply).into_owned()
    }

    fn responses(&self) -> u32 {
        self.net.status().ports().iter().map(|port| port.services.http_responses).sum()
    }

    fn events(&self) -> (usize, usize) {
        let reply = self.reply();
        (reply.matches("event:status\ndata:{\"uptime_ms\":").count(), reply.matches(":\n\n").count())
    }
}

#[rstest]
#[case::page("GET / HTTP/1.1\r\nHost: ferrite.local\r\n\r\n", "HTTP/1.1 200 \r\n", PAGE)]
#[case::document("GET /doc HTTP/1.1\r\nHost: ferrite.local\r\n\r\n", "HTTP/1.1 200 \r\n", "\"revision\":0,\"ports\":1")]
#[case::text(
    "GET /text HTTP/1.1\r\nHost: ferrite.local\r\n\r\n",
    "HTTP/1.1 200 \r\n",
    "Content-Type: text/plain; charset=utf-8\r\n"
)]
#[case::not_found("GET /favicon.ico HTTP/1.1\r\nHost: ferrite.local\r\n\r\n", "HTTP/1.1 404 \r\n", "no such resource")]
#[case::wrong_method("GET /pause HTTP/1.1\r\nHost: ferrite.local\r\n\r\n", "HTTP/1.1 405 \r\n", "Allow: POST\r\n")]
#[case::form_post(
    "POST /pause HTTP/1.1\r\nHost: ferrite.local\r\nContent-Type: text/plain\r\nContent-Length: 0\r\n\r\n",
    "HTTP/1.1 415 \r\n",
    "application/json"
)]
#[case::bad_body(
    "POST /echo HTTP/1.1\r\nHost: ferrite.local\r\nContent-Type: application/json\r\nContent-Length: 3\r\n\r\nabc",
    "HTTP/1.1 400 \r\n",
    "bad body: not a number"
)]
#[case::no_host("GET /doc HTTP/1.1\r\n\r\n", "HTTP/1.1 400 \r\n", "missing Host header")]
#[case::foreign("GET /doc HTTP/1.1\r\nHost: evil.example\r\n\r\n", "HTTP/1.1 421 \r\n", "use ferrite.local")]
fn answers(#[case] request: &str, #[case] status_line: &str, #[case] contains: &str) {
    let mut rig = Rig::new(&[1]);
    rig.connect(0, 49_152);
    rig.request(0, request);
    rig.run(200);
    let reply = rig.reply();
    check!(reply.starts_with(status_line), "{reply}");
    check!(reply.contains(contains), "{reply}");
    check!(rig.responses() == 1);
    check!(rig.commands.is_empty());
}

#[test]
fn a_control_post_queues_its_command_and_quit_waits_for_its_reply() {
    let mut rig = Rig::new(&[1]);
    rig.connect(0, 49_152);
    rig.request(
        0,
        &format!("POST /echo HTTP/1.1\r\n{HOST}Content-Type: application/json\r\nContent-Length: 2\r\n\r\n42"),
    );
    rig.run(200);
    check!(rig.reply().starts_with("HTTP/1.1 202 \r\n"));
    check!(rig.reply().ends_with("{\"accepted\":\"echo\"}"));
    check!(rig.commands == [Command::Echo(42)]);

    rig.commands.clear();
    rig.request(
        0,
        &format!("POST /quit HTTP/1.1\r\n{HOST}Content-Type: application/json\r\nContent-Length: 1\r\n\r\n0"),
    );
    rig.run(200);
    check!(rig.commands == [Command::Quit]);
}

#[test]
fn keep_alive_serves_two_requests_on_one_connection() {
    let mut rig = Rig::new(&[1]);
    rig.connect(0, 49_152);
    rig.request(0, &format!("GET /doc HTTP/1.1\r\n{HOST}\r\n"));
    rig.run(100);
    rig.request(
        0,
        &format!("POST /pause HTTP/1.1\r\n{HOST}Content-Type: application/json\r\nContent-Length: 0\r\n\r\n"),
    );
    rig.run(100);
    let reply = rig.reply();
    check!(reply.matches("HTTP/1.1 ").count() == 2);
    check!(reply.contains("HTTP/1.1 202 \r\n"));
    check!(rig.client(0).may_send(), "still open");
    check!(rig.commands == [Command::Pause]);
}

#[test]
fn an_idle_kept_alive_connection_is_closed_and_the_socket_listens_again() {
    let mut rig = Rig::new(&[1]);
    rig.connect(0, 49_152);
    rig.request(0, &format!("GET /doc HTTP/1.1\r\n{HOST}\r\n"));
    rig.run_until(0, |sock| !sock.may_recv(), 10_000);
    check!(!rig.client(0).may_recv(), "the server closed its side");
    rig.client(0).close();
    rig.run(100);

    rig.connect(0, 49_153);
    rig.request(0, &format!("GET / HTTP/1.1\r\n{HOST}\r\n"));
    rig.run(200);
    check!(rig.reply().starts_with("HTTP/1.1 200 \r\n"));
    check!(rig.responses() == 2);
}

#[test]
fn requests_close_together_share_one_render() {
    let mut rig = Rig::new(&[1]);
    rig.connect(0, 49_152);
    for _ in 0..5 {
        rig.request(0, &format!("GET /doc HTTP/1.1\r\n{HOST}\r\n"));
        rig.run(20);
    }
    check!(rig.responses() == 5);
    check!(rig.net.site().renders.get() == 1);

    rig.state.revision += 1;
    rig.request(0, &format!("GET /doc HTTP/1.1\r\n{HOST}\r\n"));
    rig.run(20);
    check!(rig.responses() == 6);
    check!(rig.net.site().renders.get() == 2);
}

#[test]
fn the_event_stream_sends_the_first_document_at_once_then_only_changes() {
    let mut rig = Rig::new(&[1]);
    rig.connect(0, 49_152);
    rig.request(0, &format!("GET /events HTTP/1.1\r\n{HOST}\r\n"));
    rig.run(2_500);
    let reply = rig.reply();
    check!(reply.starts_with("HTTP/1.1 200 \r\n"));
    check!(reply.contains("Content-Type: text/event-stream\r\n"));
    check!(rig.events() == (1, 0), "{reply}");

    rig.state.revision += 1;
    rig.run(1_100);
    check!(rig.events() == (2, 0));

    rig.run(3_000);
    check!(rig.events() == (2, 0), "an unchanged document is not sent again");

    rig.client(0).abort();
    rig.run(100);
    rig.connect(0, 49_153);
    rig.request(0, &format!("GET /doc HTTP/1.1\r\n{HOST}\r\n"));
    rig.run(100);
    check!(rig.reply().starts_with("HTTP/1.1 200 \r\n"), "the stream's socket and slot were given back");
}

#[test]
fn an_idle_stream_sends_a_comment_every_fifteen_seconds() {
    let mut rig = Rig::new(&[1]);
    rig.connect(0, 49_152);
    rig.request(0, &format!("GET /events HTTP/1.1\r\n{HOST}\r\n"));
    rig.run(14_500);
    check!(rig.events() == (1, 0));
    rig.run(1_000);
    check!(rig.events() == (1, 1));
}

#[test]
fn two_stacks_serve_one_site_each_on_its_own_address() {
    let mut rig = Rig::new(&[1, 2]);
    for at in 0..2 {
        rig.connect(at, 49_152);
        rig.request(at, &format!("GET /doc HTTP/1.1\r\nHost: 127.0.0.{}\r\n\r\n", at + 1));
        rig.run_until(at, |_| false, 100);
        check!(rig.reply().contains("\"ports\":2"), "{}", rig.reply());
    }
    let status = rig.net.status();
    check!(status.ports().len() == 2);
    check!(status.ports()[0].link.address == Some(cidr(1)));
    check!(status.ports()[1].link.address == Some(cidr(2)));
    check!(rig.net.site().renders.get() <= 2);
}

#[test]
fn the_host_check_accepts_every_stacks_address_and_stops_when_a_port_goes() {
    let mut rig = Rig::new(&[1, 2]);
    rig.connect(0, 49_152);
    // A Host naming the other stack's address passes, as does the name; a third address does not.
    for (host, status) in [("10.9.9.9", "421"), ("ferrite.local", "200"), ("127.0.0.2:80", "200")] {
        rig.reply.clear();
        rig.request(0, &format!("GET /doc HTTP/1.1\r\nHost: {host}\r\n\r\n"));
        rig.run(60);
        check!(rig.reply().starts_with(&format!("HTTP/1.1 {status} ")), "{host}: {}", rig.reply());
    }
    check!(rig.net.remove(rig.ids[1]));
    check!(!rig.net.remove(rig.ids[1]));
    rig.reply.clear();
    rig.request(0, "GET /doc HTTP/1.1\r\nHost: 192.0.2.7\r\n\r\n");
    rig.run(60);
    check!(rig.reply().starts_with("HTTP/1.1 421 "));
    check!(rig.net.status().ports().len() == 1);
}

#[test]
fn a_running_server_makes_no_allocations() {
    let mut rig = Rig::new(&[1, 2]);
    rig.connect(0, 49_152);
    let cases = [
        format!("GET /doc HTTP/1.1\r\n{HOST}\r\n"),
        format!("GET / HTTP/1.1\r\n{HOST}\r\n"),
        format!("GET /nope HTTP/1.1\r\n{HOST}\r\n"),
        "GET /doc HTTP/1.1\r\nHost: evil.example\r\n\r\n".into(),
        format!("POST /pause HTTP/1.1\r\n{HOST}Content-Type: application/json\r\nContent-Length: 0\r\n\r\n"),
        format!("POST /echo HTTP/1.1\r\n{HOST}Content-Type: application/json\r\nContent-Length: 2\r\n\r\n17"),
    ];
    for request in &cases {
        rig.request(0, request);
        rig.run(30);
        rig.commands.clear();
    }
    let (served, allocs) = (rig.responses(), rig.allocs);
    for request in cases.iter().cycle().take(60) {
        rig.state.revision += 1;
        rig.request(0, request);
        rig.run(30);
        rig.commands.clear();
    }
    check!(rig.responses() - served == 60);
    check!(rig.allocs == allocs, "{} allocations in 60 requests", rig.allocs - allocs);

    rig.client(0).abort();
    rig.run(2_000);
    rig.connect(0, 49_153);
    rig.request(0, &format!("GET /events HTTP/1.1\r\n{HOST}\r\n"));
    rig.run(1_500);
    let allocs = rig.allocs;
    for _ in 0..5 {
        rig.state.revision += 1;
        rig.run(1_100);
    }
    check!(rig.events().0 >= 5);
    check!(rig.allocs == allocs, "{} allocations in 5 events", rig.allocs - allocs);
}

#[test]
fn an_ipv4_group_mac_keeps_the_low_23_bits() {
    use crate::port::MacAddr;
    check!(MacAddr::ipv4_multicast(crate::mdns::MDNS_GROUP.octets()).octets() == [0x01, 0x00, 0x5E, 0x00, 0x00, 0xFB]);
    check!(MacAddr::ipv4_multicast([239, 255, 255, 250]).octets() == [0x01, 0x00, 0x5E, 0x7F, 0xFF, 0xFA]);
}
