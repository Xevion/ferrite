//! IP over the lab's encrypted link, in the main loop as the appliance will run it: one smoltcp
//! interface on the AP (wlan1, 10.42.0.1/24) and one on the station (wlan0), both over the frame seam.
//! The AP answers DHCP with a fixed lease (10.42.0.2/24), and once the station's DHCP client has it, the
//! station pings the AP, sends a UDP datagram the AP echoes, and fetches the AP's HTTP page over TCP; the
//! result goes back to init with `finish_session`.

use std::rc::Rc;
use std::time::Duration;

use ferrite_hal::Instant;
use ferrite_net::dhcp::DhcpClient;
use ferrite_net::http::HttpResponder;
use ferrite_net::ping::Pinger;
use ferrite_net::{QueuePort, smol_instant};
use smoltcp::iface::{Interface, SocketHandle, SocketSet};
use smoltcp::socket::{tcp, udp};
use smoltcp::wire::{DhcpMessageType, DhcpPacket, DhcpRepr, IpAddress, IpCidr, IpEndpoint, Ipv4Address, Ipv4Cidr};

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

struct Side {
    link: Rc<QueuePort>,
    iface: Interface,
    sockets: SocketSet<'static>,
}

impl Side {
    fn new(link: Rc<QueuePort>, addr: Option<Ipv4Address>, now: Instant, seed: u64) -> Self {
        let mut iface = link.interface(smol_instant(now), seed);
        if let Some(addr) = addr {
            iface.update_ip_addrs(|addrs| {
                let _ = addrs.push(IpCidr::new(IpAddress::Ipv4(addr), PREFIX));
            });
        }
        Self { link, iface, sockets: SocketSet::new(Vec::new()) }
    }

    fn poll(&mut self, now: Instant) {
        self.iface.poll(smol_instant(now), &mut self.link.device(), &mut self.sockets);
    }
}

fn udp_socket(sockets: &mut SocketSet<'static>, port: u16) -> SocketHandle {
    let buffer = || udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0; 2048]);
    let mut socket = udp::Socket::new(buffer(), buffer());
    if let Err(error) = socket.bind(port) {
        log::error!("ip: binding udp :{port} failed: {error}");
    }
    sockets.add(socket)
}

pub struct LabNet {
    station: Side,
    ap: Side,
    started: Instant,
    dhcp: DhcpClient,
    dhcp_server: SocketHandle,
    pinger: Option<Pinger>,
    station_udp: SocketHandle,
    ap_udp: SocketHandle,
    udp_sent_at: Option<Instant>,
    udp_done: bool,
    tcp: SocketHandle,
    tcp_started: bool,
    tcp_reply: Vec<u8>,
    tcp_done: bool,
    http: HttpResponder,
}

impl LabNet {
    fn new(station: Rc<QueuePort>, ap: Rc<QueuePort>, now: Instant, seed: u64) -> Self {
        let mut station = Side::new(station, None, now, seed);
        let mut ap = Side::new(ap, Some(AP_ADDR), now, seed.rotate_left(17));
        log::info!(
            "ip: station on {} asks for a lease; AP {AP_ADDR}/{PREFIX} on {} serves DHCP",
            mac_text(station.link.mac()),
            mac_text(ap.link.mac())
        );
        let dhcp = DhcpClient::new(&mut station.sockets);
        let dhcp_server = udp_socket(&mut ap.sockets, DHCP_SERVER_PORT);
        let station_udp = udp_socket(&mut station.sockets, UDP_LOCAL);
        let ap_udp = udp_socket(&mut ap.sockets, ECHO_PORT);
        let tcp = station
            .sockets
            .add(tcp::Socket::new(tcp::SocketBuffer::new(vec![0; 4096]), tcp::SocketBuffer::new(vec![0; 4096])));
        let http = HttpResponder::new(&mut ap.sockets, HTTP_PORT);
        Self {
            station,
            ap,
            started: now,
            dhcp,
            dhcp_server,
            pinger: None,
            station_udp,
            ap_udp,
            udp_sent_at: None,
            udp_done: false,
            tcp,
            tcp_started: false,
            tcp_reply: Vec::new(),
            tcp_done: false,
            http,
        }
    }

    /// One round of both stacks and the checks; `Some(passed)` once they are over.
    fn poll(&mut self, now: Instant) -> Option<bool> {
        self.station.poll(now);
        self.ap.poll(now);
        self.serve_dhcp();
        self.http.poll(&mut self.ap.sockets, || PAGE.into());
        self.udp_echo();
        let leased = self.dhcp.poll(&mut self.station.iface, &mut self.station.sockets).cloned();
        if let Some(lease) = leased
            && self.pinger.is_none()
        {
            log::info!("ip: station leased {} after {} ms", lease.address, (now - self.started).as_millis());
            self.pinger =
                Some(Pinger::new(&mut self.station.sockets, AP_ADDR, 0x1ab, PINGS, PING_INTERVAL, PING_TIMEOUT, now));
        }
        if let Some(pinger) = &mut self.pinger {
            pinger.poll(&mut self.station.sockets, now);
            self.udp(now);
            self.tcp();
        }
        self.station.poll(now);
        self.ap.poll(now);
        let pinged = self.pinger.as_ref().is_some_and(Pinger::done);
        if !(pinged && self.udp_done && self.tcp_done) && now - self.started < LIMIT {
            return None;
        }
        let leased = self.dhcp.lease().is_some_and(|lease| lease.address == Ipv4Cidr::new(STATION_ADDR, PREFIX));
        if !leased {
            log::error!("ip: the station has no lease of {STATION_ADDR}/{PREFIX} after {} ms", LIMIT.as_millis());
        }
        let replies = self.pinger.as_ref().map_or(0, |pinger| pinger.replies().count());
        log::info!("ip: ping {AP_ADDR}: {replies} of {PINGS} replies");
        let passed = leased && replies == usize::from(PINGS) && self.udp_done && self.tcp_done;
        if !self.udp_done {
            log::error!("ip: no UDP echo from {AP_ADDR}:{ECHO_PORT} within {} ms", LIMIT.as_millis());
        }
        if !self.tcp_done {
            log::error!("ip: no HTTP reply from {AP_ADDR}:{HTTP_PORT} within {} ms", LIMIT.as_millis());
        }
        log::info!("ip: frames: station {}; AP {}", self.station.link.stats(), self.ap.link.stats());
        if passed {
            log::info!("ip: lab IP checks passed over the encrypted link");
        }
        Some(passed)
    }

    fn udp_echo(&mut self) {
        let echo = self.ap.sockets.get_mut::<udp::Socket<'_>>(self.ap_udp);
        while let Ok((data, meta)) = echo.recv() {
            let data = data.to_vec();
            if let Err(error) = echo.send_slice(&data, meta.endpoint) {
                log::warn!("ip: udp echo to {} failed: {error}", meta.endpoint);
            }
        }
    }

    /// The AP's DHCP server: every DISCOVER gets an OFFER and every REQUEST an ACK of the one lease.
    fn serve_dhcp(&mut self) {
        let socket = self.ap.sockets.get_mut::<udp::Socket<'_>>(self.dhcp_server);
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
        let socket = self.station.sockets.get_mut::<udp::Socket<'_>>(self.station_udp);
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
        let cx = self.station.iface.context();
        let socket = self.station.sockets.get_mut::<tcp::Socket<'_>>(self.tcp);
        if !self.tcp_started {
            match socket.connect(cx, (IpAddress::Ipv4(AP_ADDR), HTTP_PORT), TCP_LOCAL) {
                Ok(()) => self.tcp_started = true,
                Err(error) => log::warn!("ip: tcp connect to {AP_ADDR}:{HTTP_PORT} failed: {error}"),
            }
            return;
        }
        if socket.may_send() && socket.send_queue() == 0 && self.tcp_reply.is_empty() {
            let _ = socket.send_slice(b"GET / HTTP/1.0\r\nHost: 10.42.0.1\r\n\r\n");
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
        if !socket.may_recv() && !self.tcp_reply.is_empty() {
            socket.close();
            let text = String::from_utf8_lossy(&self.tcp_reply);
            let status = text.lines().next().unwrap_or_default();
            if status.starts_with("HTTP/1.0 200") && text.ends_with(PAGE) {
                log::info!("ip: tcp: \"{status}\" from {AP_ADDR}:{HTTP_PORT}, {} bytes", self.tcp_reply.len());
                self.tcp_done = true;
            } else {
                log::error!("ip: tcp: unexpected reply from {AP_ADDR}:{HTTP_PORT}: {text:?}");
                self.tcp_reply.clear();
            }
        }
    }

    fn tx_pending(&self) -> bool {
        self.station.link.tx_pending() || self.ap.link.tx_pending()
    }
}

fn mac_text(mac: [u8; 6]) -> String {
    mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

/// The main loop's IP: takes the session init publishes, runs the checks and hands back the result.
#[derive(Default)]
pub struct MainNet {
    lab: Option<LabNet>,
}

impl MainNet {
    /// Poll once; returns how long the main loop may sleep for IP's sake.
    pub fn poll(&mut self, now: Instant, seed: u64) -> Option<Duration> {
        if self.lab.is_none()
            && let Some(session) = ferrite_wifi_start::net::take_session()
        {
            if let Some(ap) = session.ap {
                self.lab = Some(LabNet::new(session.station, ap, now, seed));
            } else {
                log::error!("ip: the lab session has no AP link");
                ferrite_wifi_start::net::finish_session(false);
            }
        }
        if ferrite_wifi_start::net::session_closed() {
            self.lab = None;
        }
        let lab = self.lab.as_mut()?;
        if let Some(passed) = lab.poll(now) {
            ferrite_wifi_start::net::finish_session(passed);
            self.lab = None;
            return None;
        }
        Some(if lab.tx_pending() { Duration::ZERO } else { IDLE_POLL })
    }
}
