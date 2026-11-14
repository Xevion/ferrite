//! `wifitrace`'s IP over the connected Wi-Fi link, run from the main loop.
//!
//! smoltcp's `DHCPv4` client, echo requests to the router, echo replies to anyone who pings (smoltcp's
//! own), and a plain-text page on TCP port 80, until the hold ends. The link arrives as a
//! [`PortEvent`] on the [`Shared`] the test script offers it through, and the verdict goes back to
//! the script with [`Shared::report`].

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::convert::Infallible;
use core::fmt::Write as _;
use core::time::Duration;

use ferrite_hal::Instant;
use ferrite_net::http::{Rejection, Route, Site};
use ferrite_net::port::MacAddr;
use ferrite_net::{NetStatus, Network, PortId, QueuePort, StackConfig};
use ferrite_wifi::{PortEvent, Shared};

/// How long the DHCP client gets to a lease.
const DHCP_LIMIT: Duration = Duration::from_secs(10);
const PINGS: u16 = 4;
const PING_INTERVAL: Duration = Duration::from_secs(1);
const PING_TIMEOUT: Duration = Duration::from_secs(1);
const PING_IDENT: u16 = 0xfe11;
const IDLE_POLL: Duration = Duration::from_millis(5);

/// What the page shows of the session, as one poll sees it.
struct Page<'a> {
    name: &'a str,
    started: Instant,
    hold: Duration,
}

/// A page of plain text at `/`; no controls.
struct TextPage;

/// The control that no route has.
#[derive(Clone, Copy)]
enum NoControl {}

impl From<NoControl> for &'static str {
    fn from(control: NoControl) -> Self {
        match control {}
    }
}

impl Site for TextPage {
    type State<'a> = Page<'a>;
    type Control = NoControl;
    type Command = ();
    type BodyError = Infallible;
    type Key = ();

    fn document_type(&self) -> &'static str {
        "text/plain; charset=utf-8"
    }

    fn page(&self) -> &'static str {
        ""
    }

    fn route(&self, method: &str, path: &str) -> Result<Route<NoControl>, Rejection> {
        match (method, path) {
            ("GET", "/") => Ok(Route::Document),
            (_, "/") => Err(Rejection::MethodNotAllowed { allow: "GET" }),
            _ => Err(Rejection::NotFound),
        }
    }

    fn command(&self, control: NoControl, _body: &[u8]) -> Result<(), Infallible> {
        match control {}
    }

    fn key(&self, _state: &Page<'_>) {}

    fn render(&self, state: &Page<'_>, net: &NetStatus, now: Instant, out: &mut String) {
        let _ = writeln!(out, "ferrite wifitrace\ninterface {}", state.name);
        if let Some(port) = net.ports().first() {
            let link = &port.link;
            if let Some(address) = link.address {
                let router = link.gateway.map_or_else(|| String::from("none"), |r| alloc::format!("{r}"));
                let _ = writeln!(out, "address {address}\nrouter {router}");
            }
            let _ = writeln!(
                out,
                "up {} s of {} s\nframes {}",
                (now - state.started).as_secs(),
                state.hold.as_secs(),
                port.frames
            );
        }
    }
}

struct Session {
    link: Rc<QueuePort>,
    name: String,
    network: Network<TextPage>,
    id: PortId,
    ping_reported: bool,
    started: Instant,
    hold: Duration,
    failure: Option<&'static str>,
}

impl Session {
    fn new(link: Rc<QueuePort>, now: Instant, seed: u64, hold: Duration) -> Self {
        let name = alloc::format!("{}", MacAddr::new(link.mac()));
        let mut network = Network::new(TextPage);
        let config = StackConfig { seed, address: None, mdns: false, http: true };
        let id = network.add(Box::new(Rc::clone(&link)), config, now);
        log::info!(
            "ip: smoltcp on {name}: DHCP for at most {} s, then the link is held until {} s after the port opened",
            DHCP_LIMIT.as_secs(),
            hold.as_secs()
        );
        Self { link, name, network, id, ping_reported: false, started: now, hold, failure: None }
    }

    /// One round of the stack; `Some(passed)` once the hold is over or a step failed early.
    fn poll(&mut self, now: Instant) -> Option<bool> {
        let page = Page { name: &self.name, started: self.started, hold: self.hold };
        self.network.poll(now, &page, &mut |()| {});
        let stack = self.network.stack_mut(self.id)?;
        let link = *stack.link();
        if let Some(cidr) = link.address
            && stack.pinger().is_none()
        {
            log::info!(
                "ip: leased after {} ms; ping {} or fetch http://{}/ until the hold ends",
                (now - self.started).as_millis(),
                cidr.address(),
                cidr.address()
            );
            if let Some(router) = link.gateway {
                stack.start_ping(router, PING_IDENT, PINGS, PING_INTERVAL, PING_TIMEOUT, now);
            } else {
                log::error!("ip: the lease names no router to ping");
                self.failure.get_or_insert("no router");
            }
        }
        if link.address.is_none() && stack.pinger().is_none() && now - self.started >= DHCP_LIMIT {
            log::error!("ip: no DHCP lease within {} s", DHCP_LIMIT.as_secs());
            return Some(self.end(Some("no DHCP lease")));
        }
        if let Some(pinger) = stack.pinger()
            && pinger.done()
            && !self.ping_reported
        {
            self.ping_reported = true;
            let rtts: Vec<Duration> = pinger.replies().collect();
            if let (Some(min), Some(max)) = (rtts.iter().min(), rtts.iter().max()) {
                let avg = rtts.iter().sum::<Duration>() / rtts.len() as u32;
                log::info!(
                    "ip: router {}: {} of {PINGS} replies, round trip min {} us, avg {} us, max {} us",
                    pinger.target(),
                    rtts.len(),
                    min.as_micros(),
                    avg.as_micros(),
                    max.as_micros()
                );
            } else {
                log::error!("ip: router {}: no reply to {PINGS} echo requests", pinger.target());
                self.failure.get_or_insert("no reply from the router");
            }
        }
        (now - self.started >= self.hold).then(|| {
            let failure = self.failure;
            self.end(failure)
        })
    }

    fn end(&self, failure: Option<&'static str>) -> bool {
        let served = self.network.stack(self.id).map_or(0, |stack| stack.status().services.http_responses);
        log::info!("ip: {served} HTTP requests answered; frames {}", self.link.stats());
        if let Some(why) = failure {
            log::error!("ip: failed: {why}");
            return false;
        }
        log::info!("ip: lease, router pings and the hold passed");
        true
    }
}

/// The main loop's IP: takes the link the script offers, runs the session and reports back.
pub struct MainNet {
    shared: Rc<Shared>,
    session: Option<Session>,
    seed: u64,
    hold: Duration,
}

impl MainNet {
    /// Hold each link for `hold` from the moment it is handed over.
    pub const fn new(shared: Rc<Shared>, seed: u64, hold: Duration) -> Self {
        Self { shared, session: None, seed, hold }
    }

    /// Poll once; returns how long the main loop may sleep for IP's sake, if a session runs.
    pub fn poll(&mut self, now: Instant) -> Option<Duration> {
        while let Some(event) = self.shared.next_port_event() {
            match event {
                PortEvent::Up(link) => self.session = Some(Session::new(link, now, self.seed, self.hold)),
                PortEvent::Down => {
                    if self.session.take().is_some() {
                        log::warn!("ip: init closed the link before the hold ended");
                    }
                },
            }
        }
        let session = self.session.as_mut()?;
        if let Some(passed) = session.poll(now) {
            self.shared.report(passed);
            self.session = None;
            return None;
        }
        let wait = session.network.stack_mut(session.id).and_then(|stack| stack.poll_delay(now));
        let wait = wait.map_or(IDLE_POLL, |d| d.min(IDLE_POLL));
        Some(if session.link.tx_pending() { Duration::ZERO } else { wait })
    }
}
