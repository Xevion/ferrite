//! `wifitrace`'s IP over the connected Wi-Fi link, run from the main loop.
//!
//! smoltcp's `DHCPv4` client, echo requests to the router, echo replies to anyone who pings (smoltcp's
//! own), and a plain-text page on TCP port 80, until the hold ends. The verdict goes back to init with
//! `finish_session`.

use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;
use core::time::Duration;

use ferrite_hal::Instant;
use ferrite_netif::dhcp::DhcpClient;
use ferrite_netif::http::HttpResponder;
use ferrite_netif::ping::Pinger;
use ferrite_netif::{Link, smol_instant};
use smoltcp::iface::{Interface, SocketSet};

/// How long the DHCP client gets to a lease.
const DHCP_LIMIT: Duration = Duration::from_secs(10);
const PINGS: u16 = 4;
const PING_INTERVAL: Duration = Duration::from_secs(1);
const PING_TIMEOUT: Duration = Duration::from_secs(1);
const PING_IDENT: u16 = 0xfe11;
const HTTP_PORT: u16 = 80;
const IDLE_POLL: Duration = Duration::from_millis(5);

struct Session {
    link: Rc<Link>,
    name: String,
    iface: Interface,
    sockets: SocketSet<'static>,
    dhcp: DhcpClient,
    http: HttpResponder,
    pinger: Option<Pinger>,
    ping_reported: bool,
    started: Instant,
    hold: Duration,
    failure: Option<&'static str>,
}

impl Session {
    fn new(link: Rc<Link>, name: String, now: Instant, seed: u64, hold: Duration) -> Self {
        let iface = link.interface(smol_instant(now), seed);
        let mut sockets = SocketSet::new(Vec::new());
        let dhcp = DhcpClient::new(&mut sockets);
        let http = HttpResponder::new(&mut sockets, HTTP_PORT);
        log::info!(
            "ip: smoltcp on {name}: DHCP for at most {} s, then the link is held until {} s after the port opened",
            DHCP_LIMIT.as_secs(),
            hold.as_secs()
        );
        Self {
            link,
            name,
            iface,
            sockets,
            dhcp,
            http,
            pinger: None,
            ping_reported: false,
            started: now,
            hold,
            failure: None,
        }
    }

    fn page(&self, now: Instant) -> String {
        let mut page = String::from("ferrite wifitrace\n");
        let _ = writeln!(page, "interface {}", self.name);
        if let Some(lease) = self.dhcp.lease() {
            let router = lease.router.map_or_else(|| String::from("none"), |r| alloc::format!("{r}"));
            let _ = writeln!(page, "address {}\nrouter {router}", lease.address);
        }
        let _ = writeln!(
            page,
            "up {} s of {} s\nframes {}",
            (now - self.started).as_secs(),
            self.hold.as_secs(),
            self.link.stats()
        );
        page
    }

    /// One round of the stack; `Some(passed)` once the hold is over or a step failed early.
    fn poll(&mut self, now: Instant) -> Option<bool> {
        self.iface.poll(smol_instant(now), &mut self.link.device(), &mut self.sockets);
        let lease = self.dhcp.poll(&mut self.iface, &mut self.sockets).cloned();
        if let Some(lease) = lease
            && self.pinger.is_none()
        {
            log::info!(
                "ip: leased after {} ms; ping {} or fetch http://{}/ until the hold ends",
                (now - self.started).as_millis(),
                lease.address.address(),
                lease.address.address()
            );
            if let Some(router) = lease.router {
                self.pinger =
                    Some(Pinger::new(&mut self.sockets, router, PING_IDENT, PINGS, PING_INTERVAL, PING_TIMEOUT, now));
            } else {
                log::error!("ip: the lease names no router to ping");
                self.failure.get_or_insert("no router");
            }
        }
        if self.dhcp.lease().is_none() && self.pinger.is_none() && now - self.started >= DHCP_LIMIT {
            log::error!("ip: no DHCP lease within {} s", DHCP_LIMIT.as_secs());
            return Some(self.end(Some("no DHCP lease")));
        }
        if let Some(pinger) = &mut self.pinger {
            pinger.poll(&mut self.sockets, now);
            if pinger.done() && !self.ping_reported {
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
        }
        let page = self.page(now);
        self.http.poll(&mut self.sockets, || page);
        self.iface.poll(smol_instant(now), &mut self.link.device(), &mut self.sockets);
        (now - self.started >= self.hold).then(|| {
            let failure = self.failure;
            self.end(failure)
        })
    }

    fn end(&self, failure: Option<&'static str>) -> bool {
        log::info!("ip: {} HTTP requests answered; frames {}", self.http.served(), self.link.stats());
        if let Some(why) = failure {
            log::error!("ip: failed: {why}");
            return false;
        }
        log::info!("ip: lease, router pings and the hold passed");
        true
    }
}

/// The main loop's IP: takes the link init publishes, runs the session and reports back.
pub struct MainNet {
    session: Option<Session>,
    seed: u64,
    hold: Duration,
}

impl MainNet {
    /// Hold each link for `hold` from the moment it is handed over.
    pub const fn new(seed: u64, hold: Duration) -> Self {
        Self { session: None, seed, hold }
    }

    /// Poll once; returns how long the main loop may sleep for IP's sake, if a session runs.
    pub fn poll(&mut self, now: Instant) -> Option<Duration> {
        if self.session.is_none()
            && let Some(session) = ferrite_wifi_start::net::take_session()
        {
            self.session = Some(Session::new(session.station, session.station_name, now, self.seed, self.hold));
        }
        if ferrite_wifi_start::net::session_closed() && self.session.take().is_some() {
            log::warn!("ip: init closed the link before the hold ended");
        }
        let session = self.session.as_mut()?;
        if let Some(passed) = session.poll(now) {
            ferrite_wifi_start::net::finish_session(passed);
            self.session = None;
            return None;
        }
        let wait = session
            .iface
            .poll_delay(smol_instant(now), &session.sockets)
            .map_or(IDLE_POLL, |d| Duration::from(d).min(IDLE_POLL));
        Some(if session.link.tx_pending() { Duration::ZERO } else { wait })
    }
}
