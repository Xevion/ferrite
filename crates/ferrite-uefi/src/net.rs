//! Wired networking: a smoltcp [`Device`] over the UEFI Simple Network Protocol,
//! plus a DHCPv4 client. This is the foundation the mDNS responder and HTTP
//! server sit on.
//!
//! SNP gives us raw L2 frame `transmit`/`receive`; smoltcp turns that into a
//! full IPv4 stack with DHCP/UDP/TCP. Note that `wait_for_packet_event` is
//! documented as never firing under QEMU, so we *poll* `receive` (it returns
//! `NOT_READY` when the queue is empty) rather than blocking on the event.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::{dhcpv4, tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address, Ipv4Cidr};
use uefi::boot::{self, ScopedProtocol};
use uefi::proto::network::snp::{NetworkState, ReceiveFlags, SimpleNetwork};

use crate::mdns;

/// Link-local mDNS multicast group and port.
const MDNS_ADDR: Ipv4Address = Ipv4Address::new(224, 0, 0, 251);
const MDNS_PORT: u16 = 5353;

/// Largest Ethernet frame we hand to/from the firmware (14B header + 1500 MTU +
/// a little slack for VLAN tags the firmware might surface).
const FRAME_CAP: usize = 1536;

/// A snapshot of link/lease state for the UI.
#[derive(Clone, Copy, Default)]
pub struct NetStatus {
    pub link_up: bool,
    pub mac: [u8; 6],
    pub address: Option<Ipv4Cidr>,
    pub gateway: Option<Ipv4Address>,
    pub dns: Option<Ipv4Address>,
    /// Inbound mDNS queries seen -- nonzero proves multicast delivery works.
    pub mdns_queries: u32,
    /// HTTP requests fully served -- nonzero proves TCP inbound works.
    pub http_requests: u32,
}

/// TCP port the status server listens on.
const HTTP_PORT: u16 = 80;

pub struct Net {
    snp: ScopedProtocol<SimpleNetwork>,
    iface: Interface,
    sockets: SocketSet<'static>,
    dhcp: SocketHandle,
    http: SocketHandle,
    http_pending: bool,
    mdns: SocketHandle,
    page: String,
    millis: u64,
    mtu: usize,
    status: NetStatus,
}

impl Net {
    /// Locate the first NIC exposing SNP, bring it up, and start DHCP.
    pub fn setup() -> uefi::Result<Self> {
        let handle = boot::get_handle_for_protocol::<SimpleNetwork>()?;
        let snp = boot::open_protocol_exclusive::<SimpleNetwork>(handle)?;

        // Drive the adapter to the Initialized state. The firmware network stack
        // may have left it partway there, so only advance from where it is.
        if snp.mode().state == NetworkState::STOPPED {
            snp.start()?;
        }
        if snp.mode().state != NetworkState::INITIALIZED {
            snp.initialize(0, 0)?;
        }
        // DHCP offers arrive as broadcast; mDNS as multicast to 224.0.0.251.
        // Add that group's MAC to the hardware filter (firmware computes it) so
        // the NIC actually delivers those frames, alongside broadcast/unicast.
        let mdns_mac = snp.mcast_ip_to_mac(false, core::net::Ipv4Addr::new(224, 0, 0, 251).into()).ok();
        let mcast_filters = mdns_mac.as_ref().map(core::slice::from_ref);
        let _ = snp.receive_filters(
            ReceiveFlags::UNICAST | ReceiveFlags::BROADCAST | ReceiveFlags::MULTICAST,
            ReceiveFlags::empty(),
            false,
            mcast_filters,
        );

        let mode = snp.mode();
        let mut mac = [0u8; 6];
        mac.copy_from_slice(&mode.current_address.0[..6]);
        let link_up = bool::from(mode.media_present) || !bool::from(mode.media_present_supported);
        let mtu = (mode.max_packet_size as usize).clamp(1514, FRAME_CAP);

        let status = NetStatus { mac, link_up, ..Default::default() };

        let mut device = SnpDevice { snp: &snp, mtu };
        let config = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
        let mut iface = Interface::new(config, &mut device, Instant::from_millis(0));
        // Subscribe to the mDNS group so smoltcp accepts its inbound multicast.
        let _ = iface.join_multicast_group(MDNS_ADDR);

        let mut sockets = SocketSet::new(vec![]);
        let dhcp = sockets.add(dhcpv4::Socket::new());

        let http = sockets
            .add(tcp::Socket::new(tcp::SocketBuffer::new(vec![0u8; 2048]), tcp::SocketBuffer::new(vec![0u8; 4096])));

        let mut mdns_sock = udp::Socket::new(
            udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0u8; 1500]),
            udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0u8; 1500]),
        );
        // Bind to the mDNS port on any local address so multicast queries land.
        let _ = mdns_sock.bind(MDNS_PORT);
        let mdns = sockets.add(mdns_sock);

        Ok(Self {
            snp,
            iface,
            sockets,
            dhcp,
            http,
            http_pending: false,
            mdns,
            page: String::from("ferrite-uefi\n"),
            millis: 0,
            mtu,
            status,
        })
    }

    /// Replace the HTML body served by the status page. The app rebuilds this
    /// each tick from live tester state.
    pub fn set_page(&mut self, page: String) {
        self.page = page;
    }

    /// Advance the stack. `elapsed_ms` is the wall time since the previous call;
    /// smoltcp only needs a monotonic-ish millisecond clock for its timers, and
    /// we drive the render loop on a fixed stall so accumulating the interval is
    /// accurate enough for DHCP.
    pub fn poll(&mut self, elapsed_ms: u64) {
        self.millis = self.millis.wrapping_add(elapsed_ms);
        let now = Instant::from_millis(self.millis as i64);

        // Re-sample link state; on real NICs the media can come up after init.
        let mode = self.snp.mode();
        self.status.link_up = bool::from(mode.media_present) || !bool::from(mode.media_present_supported);

        let mut device = SnpDevice { snp: &self.snp, mtu: self.mtu };
        self.iface.poll(now, &mut device, &mut self.sockets);

        // The DHCP `Config` borrows the socket's receive buffer (its `packet`
        // field), so extract the Copy values we need and let that borrow end
        // before mutating other fields of `self`.
        let lease = match self.sockets.get_mut::<dhcpv4::Socket>(self.dhcp).poll() {
            Some(dhcpv4::Event::Configured(c)) => Some(Some((c.address, c.router, c.dns_servers.first().copied()))),
            Some(dhcpv4::Event::Deconfigured) => Some(None),
            None => None,
        };
        match lease {
            Some(Some((address, router, dns))) => self.apply_lease(address, router, dns),
            Some(None) => self.clear_lease(),
            None => {},
        }

        self.serve_http();
        self.serve_mdns();
    }

    /// Answer mDNS `A` queries for our hostname with our current lease address.
    /// No address yet means nothing to advertise, so we just drain queries.
    fn serve_mdns(&mut self) {
        let ip = self.status.address.map(|c| c.address().octets());
        let endpoint = IpEndpoint::new(IpAddress::Ipv4(MDNS_ADDR), MDNS_PORT);
        let sock = self.sockets.get_mut::<udp::Socket>(self.mdns);

        let mut seen: u32 = 0;
        while let Ok((data, _meta)) = sock.recv() {
            seen = seen.wrapping_add(1);
            let response = ip.and_then(|ip| {
                let mut out = Vec::new();
                mdns::handle_query(data, ip, &mut out).then_some(out)
            });
            if let Some(out) = response {
                let _ = sock.send_slice(&out, endpoint);
            }
        }
        self.status.mdns_queries = self.status.mdns_queries.wrapping_add(seen);
    }

    /// Single-connection HTTP/1.0 status server. We reply on the first bytes of
    /// a request (good enough for `GET /`), then close so the socket returns to
    /// listening for the next client.
    fn serve_http(&mut self) {
        let sock = self.sockets.get_mut::<tcp::Socket>(self.http);

        if !sock.is_open() {
            let _ = sock.listen(HTTP_PORT);
        }

        if sock.can_recv() {
            // Drain whatever the client sent; we don't route on the request.
            let _ = sock.recv(|buf| (buf.len(), ()));
            self.http_pending = true;
        }

        if self.http_pending && sock.can_send() {
            let body = self.page.as_bytes();
            let header = format!(
                "HTTP/1.0 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.send_slice(header.as_bytes());
            let _ = sock.send_slice(body);
            sock.close();
            self.http_pending = false;
            self.status.http_requests = self.status.http_requests.wrapping_add(1);
        }
    }

    fn apply_lease(&mut self, address: Ipv4Cidr, router: Option<Ipv4Address>, dns: Option<Ipv4Address>) {
        self.iface.update_ip_addrs(|addrs| {
            addrs.clear();
            let _ = addrs.push(IpCidr::Ipv4(address));
        });
        if let Some(router) = router {
            let _ = self.iface.routes_mut().add_default_ipv4_route(router);
        } else {
            self.iface.routes_mut().remove_default_ipv4_route();
        }
        self.status.address = Some(address);
        self.status.gateway = router;
        self.status.dns = dns;
    }

    fn clear_lease(&mut self) {
        self.iface.update_ip_addrs(|addrs| addrs.clear());
        self.iface.routes_mut().remove_default_ipv4_route();
        self.status.address = None;
        self.status.gateway = None;
        self.status.dns = None;
    }

    #[must_use]
    pub const fn status(&self) -> NetStatus {
        self.status
    }
}

struct SnpDevice<'a> {
    snp: &'a SimpleNetwork,
    mtu: usize,
}

impl Device for SnpDevice<'_> {
    type RxToken<'a>
        = SnpRxToken
    where
        Self: 'a;
    type TxToken<'a>
        = SnpTxToken<'a>
    where
        Self: 'a;

    fn receive(&mut self, _now: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let mut frame = vec![0u8; FRAME_CAP];
        match self.snp.receive(&mut frame, None, None, None, None) {
            Ok(len) => {
                frame.truncate(len);
                Some((SnpRxToken { frame }, SnpTxToken { snp: self.snp }))
            },
            // Empty queue is the common case; anything else we simply drop.
            Err(_) => None,
        }
    }

    fn transmit(&mut self, _now: Instant) -> Option<Self::TxToken<'_>> {
        Some(SnpTxToken { snp: self.snp })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = self.mtu;
        caps
    }
}

struct SnpRxToken {
    frame: Vec<u8>,
}

impl RxToken for SnpRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.frame)
    }
}

struct SnpTxToken<'a> {
    snp: &'a SimpleNetwork,
}

impl TxToken for SnpTxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = vec![0u8; len];
        let result = f(&mut buf);
        // header_size = 0: smoltcp has already written the full Ethernet header
        // into the buffer, so transmit it verbatim.
        let _ = self.snp.transmit(0, &buf, None, None, None);
        result
    }
}
