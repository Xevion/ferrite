//! The status page over a loopback stack: its routes, controls and document through the real
//! server, and the allocation count of a running one.

use std::boxed::Box;
use std::collections::VecDeque;
use std::format;
use std::rc::Rc;
use std::string::String;
use std::vec;
use std::vec::Vec;

use assert2::check;
use ferrite_hal::Instant;
use ferrite_net::loopback::LoopbackPort;
use ferrite_net::{MacAddr, Network, PortId, StackConfig};
use ferrite_wifi::{Shared, StationState};
use rstest::rstest;
use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp;
use smoltcp::wire::{IpAddress, Ipv4Address, Ipv4Cidr};

use super::testing::{count, idle_status};
use super::*;
use crate::report::{FaultRecord, History, PassRecord};
use ferrite_efi::cores::CoreChoice;

use crate::sweep::SweepStatus;

const HOST: &str = "Host: ferrite.local\r\n";

struct Rig {
    net: Network<Console>,
    id: PortId,
    client: SocketHandle,
    ms: u64,
    status: SweepStatus,
    history: History,
    faults: VecDeque<FaultRecord>,
    commands: Commands,
    reply: Vec<u8>,
    allocs: u64,
}

impl Rig {
    fn new() -> Self {
        let wifi = Shared::new();
        wifi.set_state(StationState::Connected { ssid: "home".into(), bssid: MacAddr::new([2, 0, 0, 0, 0, 1]) });
        let mut net = Network::new(Console::new(WifiView::new(Rc::clone(&wifi))));
        let config =
            StackConfig { seed: 1, address: Some(Ipv4Cidr::new(Ipv4Address::LOCALHOST, 8)), mdns: false, http: true };
        let id = net.add(Box::new(LoopbackPort::new()), config, Instant::default());
        let client = net
            .stack_mut(id)
            .unwrap()
            .parts()
            .1
            .add(tcp::Socket::new(tcp::SocketBuffer::new(vec![0; 65536]), tcp::SocketBuffer::new(vec![0; 4096])));
        Self {
            net,
            id,
            client,
            ms: 0,
            status: idle_status(),
            history: History::default(),
            faults: VecDeque::new(),
            commands: Commands::default(),
            reply: Vec::new(),
            allocs: 0,
        }
    }

    fn client(&mut self) -> &mut tcp::Socket<'static> {
        self.net.stack_mut(self.id).unwrap().parts().1.get_mut(self.client)
    }

    fn connect(&mut self, local_port: u16) {
        self.reply.clear();
        let (iface, sockets) = self.net.stack_mut(self.id).unwrap().parts();
        sockets
            .get_mut::<tcp::Socket<'_>>(self.client)
            .connect(iface.context(), (IpAddress::v4(127, 0, 0, 1), 80), local_port)
            .unwrap();
        self.run(5);
    }

    fn run(&mut self, ms: u64) {
        for _ in 0..ms {
            self.ms += 1;
            let now = Instant::from_epoch(Duration::from_millis(self.ms));
            let snapshot = Snapshot { status: self.status, history: &self.history, faults: &self.faults };
            let (net, commands) = (&mut self.net, &mut self.commands);
            let (_, allocs) = count(|| net.poll(now, &snapshot, &mut |command| commands.push(command)));
            self.allocs += allocs;
            let (id, handle, reply) = (self.id, self.client, &mut self.reply);
            let sock = self.net.stack_mut(id).unwrap().parts().1.get_mut::<tcp::Socket<'static>>(handle);
            if sock.can_recv() {
                sock.recv(|buf| {
                    reply.extend_from_slice(buf);
                    (buf.len(), ())
                })
                .unwrap();
            }
        }
    }

    fn request(&mut self, request: &str) {
        check!(self.client().send_slice(request.as_bytes()).unwrap() == request.len());
    }

    fn reply(&self) -> String {
        String::from_utf8_lossy(&self.reply).into_owned()
    }

    fn commands(&mut self) -> Vec<Command> {
        self.commands.drain().collect()
    }

    fn responses(&self) -> u32 {
        self.net.status().ports().iter().map(|p| p.services.http_responses).sum()
    }
}

#[rstest]
#[case::page("GET / HTTP/1.1\r\nHost: ferrite.local\r\n\r\n", "HTTP/1.1 200 \r\n", "<!doctype html>")]
#[case::status(
    "GET /api/status HTTP/1.1\r\nHost: ferrite.local\r\n\r\n",
    "HTTP/1.1 200 \r\n",
    "\"preset\":\"standard\""
)]
#[case::status_by_loopback_address(
    "GET /api/status HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
    "HTTP/1.1 200 \r\n",
    "\"ports\":[{"
)]
#[case::not_found("GET /favicon.ico HTTP/1.1\r\nHost: ferrite.local\r\n\r\n", "HTTP/1.1 404 \r\n", "no such resource")]
#[case::wrong_method("GET /api/quit HTTP/1.1\r\nHost: ferrite.local\r\n\r\n", "HTTP/1.1 405 \r\n", "Allow: POST\r\n")]
#[case::bad_body(
    "POST /api/restart HTTP/1.1\r\nHost: ferrite.local\r\nContent-Type: application/json\r\nContent-Length: 17\r\n\r\n{\"preset\":\"fast\"}",
    "HTTP/1.1 400 \r\n",
    "unknown preset"
)]
#[case::foreign_control(
    "POST /api/quit HTTP/1.1\r\nHost: evil.example\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
    "HTTP/1.1 421 \r\n",
    "use ferrite.local"
)]
fn answers(#[case] request: &str, #[case] status_line: &str, #[case] contains: &str) {
    let mut rig = Rig::new();
    rig.connect(49_152);
    rig.request(request);
    rig.run(200);
    let reply = rig.reply();
    check!(reply.starts_with(status_line), "{reply}");
    check!(reply.contains(contains), "{reply}");
    check!(rig.responses() == 1);
    check!(rig.commands().is_empty());
}

#[test]
fn a_control_post_queues_its_command() {
    let mut rig = Rig::new();
    rig.connect(49_152);
    rig.request(&format!(
        "POST /api/cores HTTP/1.1\r\n{HOST}Content-Type: application/json\r\nContent-Length: 11\r\n\r\n{{\"cores\":2}}"
    ));
    rig.run(200);
    check!(rig.reply().starts_with("HTTP/1.1 202 \r\n"));
    check!(rig.reply().ends_with("{\"accepted\":\"cores\"}"));
    check!(rig.commands() == [Command::Cores(CoreChoice::Count(core::num::NonZeroUsize::new(2).unwrap()))]);
}

#[test]
fn a_running_server_makes_no_allocations() {
    let mut rig = Rig::new();
    for pass in 0..64 {
        rig.history.passes.push_back(PassRecord { pass, took: Duration::from_secs(90), faults: 3 });
        rig.faults.push_back(FaultRecord {
            fault: ferrite_mem::Fault {
                addr: ferrite_mem::CpuAddr::of(core::ptr::without_provenance::<u8>(0x4000_1008)),
                expected: !0,
                actual: !0 ^ 0x10,
            },
            test: "moving-inversions",
            cpu: 3,
            pass,
        });
    }
    rig.connect(49_152);
    let cases = [
        format!("GET /api/status HTTP/1.1\r\n{HOST}\r\n"),
        format!("GET / HTTP/1.1\r\n{HOST}\r\n"),
        format!("GET /nope HTTP/1.1\r\n{HOST}\r\n"),
        "GET /api/status HTTP/1.1\r\nHost: evil.example\r\n\r\n".into(),
        format!("POST /api/pause HTTP/1.1\r\n{HOST}Content-Type: application/json\r\nContent-Length: 0\r\n\r\n"),
        format!(
            "POST /api/restart HTTP/1.1\r\n{HOST}Content-Type: application/json\r\nContent-Length: 17\r\n\r\n{{\"preset\":\"fast\"}}"
        ),
    ];
    for request in &cases {
        rig.request(request);
        rig.run(30);
        rig.commands();
    }
    let (served, allocs) = (rig.responses(), rig.allocs);
    for request in cases.iter().cycle().take(60) {
        rig.history.revision += 1;
        rig.request(request);
        rig.run(30);
        rig.commands();
    }
    check!(rig.responses() - served == 60);
    check!(rig.allocs == allocs, "{} allocations in 60 requests", rig.allocs - allocs);
}
