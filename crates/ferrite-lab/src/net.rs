//! IP over the lab's encrypted link, in the main loop as the appliance will run it: one smoltcp
//! interface on the AP (wlan1, 10.42.0.1/24) and one on the station (wlan0), both over the frame seam.
//! The AP answers DHCP with a fixed lease (10.42.0.2/24), and once the station's DHCP client has it, the
//! station pings the AP, sends a UDP datagram the AP echoes, and fetches the AP's HTTP page over TCP; the
//! result goes back with `Shared::report`.

use std::convert::Infallible;
use std::rc::Rc;
use std::time::Duration;

use ferrite_hal::Instant;
use ferrite_net::http::{Rejection, Route, Site};
use ferrite_net::port::MacAddr;
use ferrite_net::{NetStatus, Network, PortId, QueuePort, Stack, StackConfig};
use ferrite_wifi::{PortEvent, Shared};
use smoltcp::iface::SocketHandle;
use smoltcp::socket::{tcp, udp};
use smoltcp::wire::{DhcpMessageType, DhcpPacket, DhcpRepr, IpAddress, IpEndpoint, Ipv4Address, Ipv4Cidr};

const AP_ADDR: Ipv4Address = Ipv4Address::new(10, 42, 0, 1);
const STATION_ADDR: Ipv4Address = Ipv4Address::new(10, 42, 0, 2);
const PREFIX: u8 = 24;
const ECHO_PORT: u16 = 7;
const DHCP_SERVER_PORT: u16 = 67;
const DHCP_CLIENT_PORT: u16 = 68;
const LEASE_SECS: u32 = 3600;
const HTTP_PORT: u16 = 80;
const UDP_LOCAL: u16 = 40_000;
const TCP_LOCAL: u16 = 40_001;
const UDP_PAYLOAD: &[u8] = b"ferrite-lab udp echo";
const UDP_RETRY: Duration = Duration::from_millis(500);
const PINGS: u16 = 3;
const PING_INTERVAL: Duration = Duration::from_millis(200);
const PING_TIMEOUT: Duration = Duration::from_secs(1);
/// The whole exchange must finish within this, or it fails.
const LIMIT: Duration = Duration::from_secs(8);
const IDLE_POLL: Duration = Duration::from_millis(5);
const PAGE: &str = "ferrite lab AP\n";

/// The AP's page: `PAGE` as plain text at `/`.
struct ApPage;

/// The control that no route has.
#[derive(Clone, Copy)]
struct NoControl(Infallible);

impl From<NoControl> for &'static str {
    fn from(control: NoControl) -> Self {
        match control.0 {}
    }
}

impl Site for ApPage {
    type State<'a> = ();
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
        match control.0 {}
    }

    fn key(&self, _state: &()) {}

    fn render(&self, _state: &(), _net: &NetStatus, _now: Instant, out: &mut String) {
        out.push_str(PAGE);
    }
}

fn udp_socket(stack: &mut Stack, port: u16) -> SocketHandle {
    let buffer = || udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0; 2048]);
    let mut socket = udp::Socket::new(buffer(), buffer());
    if let Err(error) = socket.bind(port) {
        log::error!("ip: binding udp :{port} failed: {error}");
    }
    stack.parts().1.add(socket)
}

#[expect(clippy::struct_excessive_bools, reason = "the independent progress flags of the checks")]
pub struct LabNet {
    network: Network<ApPage>,
    station: PortId,
    ap: PortId,
    station_link: Rc<QueuePort>,
    ap_link: Rc<QueuePort>,
    started: Instant,
    dhcp_server: SocketHandle,
    pinging: bool,
    station_udp: SocketHandle,
    ap_udp: SocketHandle,
    udp_sent_at: Option<Instant>,
    udp_done: bool,
    tcp: SocketHandle,
    tcp_started: bool,
    tcp_reply: Vec<u8>,
    tcp_done: bool,
}

impl LabNet {
    fn new(station: Rc<QueuePort>, ap: Rc<QueuePort>, now: Instant, seed: u64) -> Self {
        let mut network = Network::new(ApPage);
        let station_config = StackConfig { seed, address: None, mdns: false, http: false };
        let ap_config = StackConfig {
            seed: seed.rotate_left(17),
            address: Some(Ipv4Cidr::new(AP_ADDR, PREFIX)),
            mdns: false,
            http: true,
        };
        let station_id = network.add(Box::new(Rc::clone(&station)), station_config, now);
        let ap_id = network.add(Box::new(Rc::clone(&ap)), ap_config, now);
        log::info!(
            "ip: station on {} asks for a lease; AP {AP_ADDR}/{PREFIX} on {} serves DHCP",
            MacAddr::new(station.mac()),
            MacAddr::new(ap.mac())
        );
        let station_stack = network.stack_mut(station_id).expect("the station stack was just added");
        let station_udp = udp_socket(station_stack, UDP_LOCAL);
        let tcp = station_stack
            .parts()
            .1
            .add(tcp::Socket::new(tcp::SocketBuffer::new(vec![0; 4096]), tcp::SocketBuffer::new(vec![0; 4096])));
        let ap_stack = network.stack_mut(ap_id).expect("the AP stack was just added");
        let dhcp_server = udp_socket(ap_stack, DHCP_SERVER_PORT);
        let ap_udp = udp_socket(ap_stack, ECHO_PORT);
        Self {
            network,
            station: station_id,
            ap: ap_id,
            station_link: station,
            ap_link: ap,
            started: now,
            dhcp_server,
            pinging: false,
            station_udp,
            ap_udp,
            udp_sent_at: None,
            udp_done: false,
            tcp,
            tcp_started: false,
            tcp_reply: Vec::new(),
            tcp_done: false,
        }
    }

    fn station(&mut self) -> &mut Stack {
        self.network.stack_mut(self.station).expect("the station stack lives as long as the session")
    }

    fn ap(&mut self) -> &mut Stack {
        self.network.stack_mut(self.ap).expect("the AP stack lives as long as the session")
    }

    fn step(&mut self, now: Instant) {
        self.network.poll(now, &(), &mut |()| {});
    }

    /// One round of both stacks and the checks; `Some(passed)` once they are over.
    fn poll(&mut self, now: Instant) -> Option<bool> {
        self.step(now);
        self.serve_dhcp();
        self.udp_echo();
        let leased = self.station().link().address;
        if let Some(cidr) = leased
            && !self.pinging
        {
            log::info!("ip: station leased {cidr} after {} ms", (now - self.started).as_millis());
            self.pinging = true;
            self.station().start_ping(AP_ADDR, 0x1ab, PINGS, PING_INTERVAL, PING_TIMEOUT, now);
        }
        if self.pinging {
            self.udp(now);
            self.tcp();
        }
        self.step(now);
        let pinged = self.station().pinger().is_some_and(ferrite_net::ping::Pinger::done);
        if !(pinged && self.udp_done && self.tcp_done) && now - self.started < LIMIT {
            return None;
        }
        let leased = self.station().link().address == Some(Ipv4Cidr::new(STATION_ADDR, PREFIX));
        if !leased {
            log::error!("ip: the station has no lease of {STATION_ADDR}/{PREFIX} after {} ms", LIMIT.as_millis());
        }
        let replies = self.station().pinger().map_or(0, |pinger| pinger.replies().count());
        log::info!("ip: ping {AP_ADDR}: {replies} of {PINGS} replies");
        let passed = leased && replies == usize::from(PINGS) && self.udp_done && self.tcp_done;
        if !self.udp_done {
            log::error!("ip: no UDP echo from {AP_ADDR}:{ECHO_PORT} within {} ms", LIMIT.as_millis());
        }
        if !self.tcp_done {
            log::error!("ip: no HTTP reply from {AP_ADDR}:{HTTP_PORT} within {} ms", LIMIT.as_millis());
        }
        log::info!("ip: frames: station {}; AP {}", self.station_link.stats(), self.ap_link.stats());
        if passed {
            log::info!("ip: lab IP checks passed over the encrypted link");
        }
        Some(passed)
    }

    fn udp_echo(&mut self) {
        let handle = self.ap_udp;
        let echo = self.ap().parts().1.get_mut::<udp::Socket<'_>>(handle);
        while let Ok((data, meta)) = echo.recv() {
            let data = data.to_vec();
            if let Err(error) = echo.send_slice(&data, meta.endpoint) {
                log::warn!("ip: udp echo to {} failed: {error}", meta.endpoint);
            }
        }
    }

    /// The AP's DHCP server: every DISCOVER gets an OFFER and every REQUEST an ACK of the one lease.
    fn serve_dhcp(&mut self) {
        let handle = self.dhcp_server;
        let socket = self.ap().parts().1.get_mut::<udp::Socket<'_>>(handle);
        let mut out = [0_u8; 576];
        while let Ok((data, _)) = socket.recv() {
            let data = data.to_vec();
            let Ok(packet) = DhcpPacket::new_checked(&data[..]) else { continue };
            let Ok(request) = DhcpRepr::parse(&packet) else { continue };
            let message_type = match request.message_type {
                DhcpMessageType::Discover => DhcpMessageType::Offer,
                DhcpMessageType::Request => DhcpMessageType::Ack,
                _ => continue,
            };
            let reply = DhcpRepr {
                message_type,
                transaction_id: request.transaction_id,
                secs: 0,
                client_hardware_address: request.client_hardware_address,
                client_ip: Ipv4Address::UNSPECIFIED,
                your_ip: STATION_ADDR,
                server_ip: AP_ADDR,
                router: Some(AP_ADDR),
                subnet_mask: Some(Ipv4Cidr::new(AP_ADDR, PREFIX).netmask()),
                relay_agent_ip: Ipv4Address::UNSPECIFIED,
                broadcast: true,
                requested_ip: None,
                client_identifier: None,
                server_identifier: Some(AP_ADDR),
                parameter_request_list: None,
                dns_servers: None,
                max_size: None,
                lease_duration: Some(LEASE_SECS),
                renew_duration: None,
                rebind_duration: None,
                additional_options: &[],
            };
            let len = reply.buffer_len().min(out.len());
            if reply.emit(&mut DhcpPacket::new_unchecked(&mut out[..len])).is_err() {
                continue;
            }
            let to = IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::BROADCAST), DHCP_CLIENT_PORT);
            match socket.send_slice(&out[..len], to) {
                Ok(()) => log::info!(
                    "ip: dhcp server: {:?} answered with {message_type:?} of {STATION_ADDR}",
                    request.message_type
                ),
                Err(error) => log::warn!("ip: dhcp server: the {message_type:?} was not sent: {error}"),
            }
        }
    }

    fn udp(&mut self, now: Instant) {
        if self.udp_done {
            return;
        }
        let handle = self.station_udp;
        let station = self.network.stack_mut(self.station).expect("the station stack");
        let socket = station.parts().1.get_mut::<udp::Socket<'_>>(handle);
        while let Ok((data, meta)) = socket.recv() {
            if data == UDP_PAYLOAD {
                log::info!("ip: udp echo through {} came back: {} bytes", meta.endpoint, data.len());
                self.udp_done = true;
                return;
            }
        }
        if self.udp_sent_at.is_none_or(|at| now - at >= UDP_RETRY) {
            let to = IpEndpoint::new(IpAddress::Ipv4(AP_ADDR), ECHO_PORT);
            if socket.send_slice(UDP_PAYLOAD, to).is_ok() {
                self.udp_sent_at = Some(now);
            }
        }
    }

    fn tcp(&mut self) {
        if self.tcp_done {
            return;
        }
        let (handle, started) = (self.tcp, self.tcp_started);
        let (iface, sockets) = self.network.stack_mut(self.station).expect("the station stack").parts();
        let cx = iface.context();
        let socket = sockets.get_mut::<tcp::Socket<'_>>(handle);
        if !started {
            match socket.connect(cx, (IpAddress::Ipv4(AP_ADDR), HTTP_PORT), TCP_LOCAL) {
                Ok(()) => self.tcp_started = true,
                Err(error) => log::warn!("ip: tcp connect to {AP_ADDR}:{HTTP_PORT} failed: {error}"),
            }
            return;
        }
        if socket.may_send() && socket.send_queue() == 0 && self.tcp_reply.is_empty() {
            let _ = socket.send_slice(b"GET / HTTP/1.1\r\nHost: 10.42.0.1\r\nConnection: close\r\n\r\n");
        }
        while socket.can_recv() {
            let reply = &mut self.tcp_reply;
            if socket
                .recv(|data| {
                    reply.extend_from_slice(data);
                    (data.len(), ())
                })
                .is_err()
            {
                break;
            }
        }
        if !response_complete(&self.tcp_reply) {
            return;
        }
        socket.close();
        let text = String::from_utf8_lossy(&self.tcp_reply);
        let status = text.lines().next().unwrap_or_default();
        if status.starts_with("HTTP/1.1 200") && text.ends_with(PAGE) {
            log::info!("ip: tcp: \"{status}\" from {AP_ADDR}:{HTTP_PORT}, {} bytes", self.tcp_reply.len());
            self.tcp_done = true;
        } else {
            log::error!("ip: tcp: unexpected reply from {AP_ADDR}:{HTTP_PORT}: {text:?}");
            self.tcp_reply.clear();
        }
    }

    fn tx_pending(&self) -> bool {
        self.station_link.tx_pending() || self.ap_link.tx_pending()
    }
}

/// Whether `reply` holds a whole response: its head and as many body bytes as `Content-Length` says.
fn response_complete(reply: &[u8]) -> bool {
    let Some(head_end) = reply.windows(4).position(|w| w == b"\r\n\r\n") else { return false };
    let head = String::from_utf8_lossy(&reply[..head_end]);
    let length = head
        .lines()
        .find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>()))
        .and_then(Result::ok)
        .unwrap_or(0);
    reply.len() >= head_end + 4 + length
}

/// The main loop's IP: takes the links the station and the lab AP offer, runs the checks over each new
/// station link against the AP's, and reports the result through the station's `Shared`.
pub struct MainNet {
    station: Rc<Shared>,
    ap: Rc<Shared>,
    seed: u64,
    station_link: Option<Rc<QueuePort>>,
    ap_link: Option<Rc<QueuePort>>,
    lab: Option<LabNet>,
    /// Whether the station link that is up has had its verdict.
    checked: bool,
    /// How many station links have come up.
    links: u32,
}

impl MainNet {
    pub const fn new(station: Rc<Shared>, ap: Rc<Shared>, seed: u64) -> Self {
        Self { station, ap, seed, station_link: None, ap_link: None, lab: None, checked: false, links: 0 }
    }

    /// How many station links have come up since the start.
    pub const fn links(&self) -> u32 {
        self.links
    }

    /// Whether a station link is up now.
    pub const fn link_up(&self) -> bool {
        self.station_link.is_some()
    }

    /// Poll once; returns how long the main loop may sleep for IP's sake.
    pub fn poll(&mut self, now: Instant) -> Option<Duration> {
        while let Some(event) = self.ap.next_port_event() {
            self.ap_link = match event {
                PortEvent::Up(link) => Some(link),
                PortEvent::Down => None,
            };
            self.lab = None;
        }
        while let Some(event) = self.station.next_port_event() {
            self.station_link = match event {
                PortEvent::Up(link) => {
                    self.checked = false;
                    self.links += 1;
                    Some(link)
                },
                PortEvent::Down => None,
            };
            self.lab = None;
        }
        if self.lab.is_none()
            && !self.checked
            && let (Some(station), Some(ap)) = (&self.station_link, &self.ap_link)
        {
            self.lab = Some(LabNet::new(Rc::clone(station), Rc::clone(ap), now, self.seed));
        }
        let lab = self.lab.as_mut()?;
        if let Some(passed) = lab.poll(now) {
            self.station.report(passed);
            self.checked = true;
            self.lab = None;
            return None;
        }
        Some(if lab.tx_pending() { Duration::ZERO } else { IDLE_POLL })
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    /// Carry every frame one side queued to the other, as the frame seam does.
    fn wire(from: &QueuePort, to: &QueuePort) {
        while let Some(frame) = from.next_tx() {
            from.transmitted(true);
            to.deliver(&frame);
        }
    }

    #[test]
    fn the_station_leases_pings_and_fetches_the_page_from_the_ap() {
        let station = Rc::new(QueuePort::new([2, 0, 0, 0, 0, 1], 1500, 32, 32));
        let ap = Rc::new(QueuePort::new([2, 0, 0, 0, 0, 2], 1500, 32, 32));
        let mut lab = LabNet::new(Rc::clone(&station), Rc::clone(&ap), Instant::default(), 7);
        let mut verdict = None;
        for ms in 1..=LIMIT.as_millis() as u64 {
            let now = Instant::from_epoch(Duration::from_millis(ms));
            verdict = lab.poll(now);
            wire(&station, &ap);
            wire(&ap, &station);
            if verdict.is_some() {
                break;
            }
        }
        check!(verdict == Some(true));
    }
}
