//! Wired networking: smoltcp over the firmware's Simple Network Protocol, with
//! a DHCP client, the mDNS responder and the HTTP status server on top.
//!
//! `wait_for_packet_event` never fires under QEMU, so the stack polls
//! `receive` (empty queue is `NOT_READY`) instead of waiting on the event.

mod device;
pub mod http;

use alloc::vec;
use alloc::vec::Vec;
use core::fmt;
use core::time::Duration;

use smoltcp::iface::{Config, Interface, PollResult, SocketHandle, SocketSet};
use smoltcp::socket::{dhcpv4, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address, Ipv4Cidr};
use snafu::{OptionExt as _, ResultExt as _, Snafu};
use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams};
use uefi::proto::network::EfiMacAddr;
use uefi::proto::network::snp::{NetworkState, ReceiveFlags, SimpleNetwork};
use uefi::proto::rng::Rng;
use uefi::{Handle, Status};

use self::device::SnpDevice;
use self::http::HttpServer;
use crate::mdns::{self, Destination, Handled, Responder};
use crate::sweep::SweepStatus;

const MDNS_ADDR: Ipv4Address = Ipv4Address::new(224, 0, 0, 251);
const MDNS_BUFFER: usize = 1500;
const MDNS_PACKETS: usize = 4;

/// Longest gap between polls when smoltcp has no timer due sooner.
const IDLE_POLL: Duration = Duration::from_millis(5);
/// Gap between polls while frames are arriving or sockets are changing.
const BUSY_POLL: Duration = Duration::from_millis(1);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MacAddr([u8; 6]);

impl MacAddr {
    #[must_use]
    pub const fn new(octets: [u8; 6]) -> Self {
        Self(octets)
    }

    #[must_use]
    pub const fn octets(self) -> [u8; 6] {
        self.0
    }
}

impl fmt::Display for MacAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [first, rest @ ..] = self.0;
        write!(f, "{first:02x}")?;
        rest.iter().try_for_each(|octet| write!(f, ":{octet:02x}"))
    }
}

/// An optional address, shown as `-` when absent.
pub struct OptAddr(pub Option<Ipv4Address>);

impl fmt::Display for OptAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(addr) => write!(f, "{addr}"),
            None => f.write_str("-"),
        }
    }
}

/// Link and lease state: what changes rarely and is worth logging when it does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Link {
    pub up: bool,
    pub mac: MacAddr,
    pub address: Option<Ipv4Cidr>,
    pub gateway: Option<Ipv4Address>,
    pub dns: Option<Ipv4Address>,
}

impl fmt::Display for Link {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "mac={} link={} ip=", self.mac, if self.up { "up" } else { "down" })?;
        match self.address {
            Some(cidr) => write!(f, "{cidr}")?,
            None => f.write_str("-")?,
        }
        write!(f, " gw={} dns={}", OptAddr(self.gateway), OptAddr(self.dns))
    }
}

/// Traffic counters since setup.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// mDNS queries (QR clear) received, for any name.
    pub mdns_queries: u32,
    pub mdns_replies: u32,
    pub mdns_malformed: u32,
    pub http_responses: u32,
    pub rx_frames: u32,
    pub rx_errors: u32,
    pub tx_errors: u32,
    pub tx_stale_reclaims: u32,
}

impl fmt::Display for Counters {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "mdns_queries={} mdns_replies={} mdns_malformed={} http={} rx={} rx_err={} tx_err={} tx_stale={}",
            self.mdns_queries,
            self.mdns_replies,
            self.mdns_malformed,
            self.http_responses,
            self.rx_frames,
            self.rx_errors,
            self.tx_errors,
            self.tx_stale_reclaims
        )
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NetStatus {
    pub link: Link,
    pub counters: Counters,
}

impl fmt::Display for NetStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.link, self.counters)
    }
}

/// The setup step that failed, and on which SNP handle.
#[derive(Debug, Snafu)]
pub enum NetError {
    #[snafu(display("locating SNP handles: {source}"))]
    Locate { source: uefi::Error },
    #[snafu(display("no SNP handle"))]
    NoNic,
    #[snafu(display("opening SNP {handle:?} exclusively: {source}"))]
    Open { handle: Handle, source: uefi::Error },
    #[snafu(display("starting SNP {handle:?}: {source}"))]
    Start { handle: Handle, source: uefi::Error },
    #[snafu(display("initializing SNP {handle:?}: {source}"))]
    Initialize { handle: Handle, source: uefi::Error },
    #[snafu(display("setting receive filters on SNP {handle:?} (supported {supported:?}): {source}"))]
    Filter { handle: Handle, supported: ReceiveFlags, source: uefi::Error },
}

impl NetError {
    /// A short reason for the UI.
    pub fn reason(&self) -> &'static str {
        let status = match self {
            Self::NoNic => return "no SNP device (no NIC / OVMF lacks net stack)",
            Self::Locate { source }
            | Self::Open { source, .. }
            | Self::Start { source, .. }
            | Self::Initialize { source, .. }
            | Self::Filter { source, .. } => source.status(),
        };
        match (self, status) {
            (_, Status::NOT_FOUND) => "no SNP device (no NIC / OVMF lacks net stack)",
            (_, Status::ACCESS_DENIED) => "SNP busy (held by firmware net stack)",
            (_, Status::UNSUPPORTED) => "SNP unsupported by firmware",
            (Self::Filter { .. }, _) => "NIC refused receive filters",
            _ => "SNP setup failed",
        }
    }
}

/// Reconnects the firmware's drivers to the SNP handle on drop. Opening SNP
/// exclusively stops the drivers above it (MNP and the IP stack).
struct Reconnect(Handle);

impl Drop for Reconnect {
    fn drop(&mut self) {
        match boot::connect_controller(self.0, &[], None, true) {
            Ok(()) => log::debug!("net: firmware drivers reconnected to SNP"),
            Err(e) => log::warn!("net: reconnecting firmware drivers to SNP failed: {e}"),
        }
    }
}

pub struct Net {
    device: SnpDevice,
    iface: Interface,
    sockets: SocketSet<'static>,
    dhcp: SocketHandle,
    http: HttpServer,
    mdns: SocketHandle,
    responder: Responder,
    mdns_out: Vec<u8>,
    status: NetStatus,
    /// Declared last so it runs after `device` has closed the protocol.
    _reconnect: Reconnect,
}

impl Net {
    /// Open the best SNP handle, bring the adapter up, and start DHCP.
    pub fn setup(now: Duration) -> Result<Self, NetError> {
        let handle = pick_nic()?;
        let reconnect = Reconnect(handle);
        let snp = boot::open_protocol_exclusive::<SimpleNetwork>(handle).context(OpenSnafu { handle })?;
        let initial_state = snp.mode().state;
        let mut device = SnpDevice::new(snp, initial_state);

        let snp = device.snp();
        if snp.mode().state == NetworkState::STOPPED {
            snp.start().context(StartSnafu { handle })?;
        }
        if snp.mode().state != NetworkState::INITIALIZED {
            snp.initialize(0, 0).context(InitializeSnafu { handle })?;
        }
        configure_filters(handle, snp)?;

        let mut octets = [0; 6];
        octets.copy_from_slice(&snp.mode().current_address.0[..6]);
        let mac = MacAddr::new(octets);
        let link = Link { up: device.link_up(), mac, ..Link::default() };
        log::debug!("net: SNP opened in state {initial_state:?}, {link}");

        let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac.octets())));
        config.random_seed = random_seed();
        let mut iface = Interface::new(config, &mut device, instant(now));
        if let Err(e) = iface.join_multicast_group(MDNS_ADDR) {
            log::warn!("net: joining {MDNS_ADDR} failed, mDNS disabled: {e}");
        }

        let mut sockets = SocketSet::new(vec![]);
        let dhcp = sockets.add(dhcpv4::Socket::new());
        let http = HttpServer::new(&mut sockets);
        let mut mdns_sock = udp::Socket::new(
            udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; MDNS_PACKETS], vec![0; MDNS_BUFFER]),
            udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; MDNS_PACKETS], vec![0; MDNS_BUFFER]),
        );
        if let Err(e) = mdns_sock.bind(mdns::MDNS_PORT) {
            log::warn!("net: binding udp :{} failed, mDNS disabled: {e}", mdns::MDNS_PORT);
        }
        let mdns = sockets.add(mdns_sock);

        Ok(Self {
            device,
            iface,
            sockets,
            dhcp,
            http,
            mdns,
            responder: Responder::new(),
            mdns_out: Vec::with_capacity(MDNS_BUFFER),
            status: NetStatus { link, counters: Counters::default() },
            _reconnect: reconnect,
        })
    }

    /// Advance the stack to `now`. Returns how long may pass before the next poll.
    pub fn poll(&mut self, now: Duration, sweep: &SweepStatus) -> Duration {
        let at = instant(now);
        self.device.reclaim(now);
        self.status.link.up = self.device.link_up();

        let rx_before = self.device.stats.rx_frames;
        let changed = self.iface.poll(at, &mut self.device, &mut self.sockets) == PollResult::SocketStateChanged;
        self.poll_dhcp(now);

        let snapshot = &self.status;
        let served = self.http.poll(&mut self.sockets, now, || http::status_page(snapshot, sweep));
        self.status.counters.http_responses = self.status.counters.http_responses.wrapping_add(served);
        self.serve_mdns(now);

        let stats = self.device.stats;
        let counters = &mut self.status.counters;
        counters.rx_frames = stats.rx_frames;
        counters.rx_errors = stats.rx_errors;
        counters.tx_errors = stats.tx_errors;
        counters.tx_stale_reclaims = stats.tx_stale_reclaims;

        if changed || stats.rx_frames != rx_before {
            return BUSY_POLL;
        }
        self.iface.poll_delay(at, &self.sockets).map_or(IDLE_POLL, |d| Duration::from(d).min(IDLE_POLL))
    }

    fn poll_dhcp(&mut self, now: Duration) {
        // The event borrows the socket, so copy out what we need first.
        let lease = match self.sockets.get_mut::<dhcpv4::Socket<'_>>(self.dhcp).poll() {
            Some(dhcpv4::Event::Configured(c)) => Some(Some((c.address, c.router, c.dns_servers.first().copied()))),
            Some(dhcpv4::Event::Deconfigured) => Some(None),
            None => None,
        };
        match lease {
            Some(Some((address, router, dns))) => self.apply_lease(now, address, router, dns),
            Some(None) => self.clear_lease(),
            None => {},
        }
    }

    /// Answer queued mDNS queries and send any due announcement.
    fn serve_mdns(&mut self, now: Duration) {
        let ip = self.status.link.address.map(|c| c.address());
        let group = IpEndpoint::new(IpAddress::Ipv4(MDNS_ADDR), mdns::MDNS_PORT);
        let sock = self.sockets.get_mut::<udp::Socket<'_>>(self.mdns);
        let counters = &mut self.status.counters;
        let out = &mut self.mdns_out;

        if let Some(ip) = ip
            && self.responder.announcement(now, ip, out)
        {
            send_mdns(sock, out, group, counters);
        }
        while let Ok((data, meta)) = sock.recv() {
            let source = meta.endpoint;
            match self.responder.handle(data, source.port, now, ip, out) {
                Ok(Handled::Response) => {},
                Ok(handled) => {
                    counters.mdns_queries = counters.mdns_queries.wrapping_add(1);
                    if let Handled::Reply(dest) = handled {
                        let to = if dest == Destination::Multicast { group } else { source };
                        send_mdns(sock, out, to, counters);
                    }
                },
                Err(e) => {
                    counters.mdns_malformed = counters.mdns_malformed.wrapping_add(1);
                    log::debug!("mdns: malformed packet from {source}: {e}");
                },
            }
        }
    }

    fn apply_lease(&mut self, now: Duration, address: Ipv4Cidr, router: Option<Ipv4Address>, dns: Option<Ipv4Address>) {
        self.iface.update_ip_addrs(|addrs| {
            addrs.clear();
            let _ = addrs.push(IpCidr::Ipv4(address));
        });
        match router {
            Some(router) => {
                if let Err(e) = self.iface.routes_mut().add_default_ipv4_route(router) {
                    log::warn!("net: adding default route via {router} failed: {e:?}");
                }
            },
            None => {
                self.iface.routes_mut().remove_default_ipv4_route();
            },
        }
        let link = &mut self.status.link;
        if link.address != Some(address) {
            self.responder.schedule_announcements(now);
        }
        link.address = Some(address);
        link.gateway = router;
        link.dns = dns;
    }

    fn clear_lease(&mut self) {
        #[expect(
            clippy::redundant_closure_for_method_calls,
            reason = "the method's crate, heapless, is not a dependency"
        )]
        self.iface.update_ip_addrs(|addrs| addrs.clear());
        self.iface.routes_mut().remove_default_ipv4_route();
        let link = &mut self.status.link;
        link.address = None;
        link.gateway = None;
        link.dns = None;
    }

    #[must_use]
    pub const fn status(&self) -> NetStatus {
        self.status
    }
}

fn send_mdns(sock: &mut udp::Socket<'_>, out: &[u8], to: IpEndpoint, counters: &mut Counters) {
    match sock.send_slice(out, to) {
        Ok(()) => counters.mdns_replies = counters.mdns_replies.wrapping_add(1),
        Err(e) => log::debug!("mdns: reply to {to} dropped: {e}"),
    }
}

fn instant(now: Duration) -> Instant {
    Instant::from_micros(i64::try_from(now.as_micros()).unwrap_or(i64::MAX))
}

/// The first SNP handle with media present, else the first one.
fn pick_nic() -> Result<Handle, NetError> {
    let handles = boot::find_handles::<SimpleNetwork>().context(LocateSnafu)?;
    let present = handles.iter().copied().find(|&h| media_present(h));
    let chosen = present.or_else(|| handles.first().copied()).context(NoNicSnafu)?;
    log::debug!("net: {} SNP handles, media present on chosen one: {}", handles.len(), present.is_some());
    Ok(chosen)
}

fn media_present(handle: Handle) -> bool {
    let params = OpenProtocolParams { handle, agent: boot::image_handle(), controller: None };
    // SAFETY: only the mode is read, and the interface is closed before anything opens it exclusively.
    let snp = unsafe { boot::open_protocol::<SimpleNetwork>(params, OpenProtocolAttributes::GetProtocol) };
    snp.is_ok_and(|snp| bool::from(snp.mode().media_present))
}

/// The Ethernet group address for an IPv4 multicast group: 01:00:5e and the group's low 23 bits.
fn ipv4_group_mac(group: Ipv4Address) -> EfiMacAddr {
    let [_, b, c, d] = group.octets();
    let mut mac = EfiMacAddr([0; 32]);
    mac.0[..6].copy_from_slice(&[0x01, 0x00, 0x5E, b & 0x7F, c, d]);
    mac
}

/// Enable unicast and broadcast, plus the best multicast mode the NIC supports:
/// the mDNS group in the hardware filter, else all multicast, else promiscuous.
/// MULTICAST is tried even when the mode reports no filter slots, because
/// iPXE's SNP never fills in that count and accepts any filter list.
fn configure_filters(handle: Handle, snp: &SimpleNetwork) -> Result<(), NetError> {
    let mode = snp.mode();
    let supported = ReceiveFlags::from_bits_truncate(mode.receive_filter_mask);
    let base = (ReceiveFlags::UNICAST | ReceiveFlags::BROADCAST) & supported;
    let group = [ipv4_group_mac(MDNS_ADDR)];
    log::debug!(
        "net: receive filters supported {supported:?}, {} multicast slots, currently {:?}",
        mode.max_mcast_filter_count,
        ReceiveFlags::from_bits_truncate(mode.receive_filter_setting)
    );

    let candidates = [
        (ReceiveFlags::MULTICAST, Some(&group[..])),
        (ReceiveFlags::PROMISCUOUS_MULTICAST, None),
        (ReceiveFlags::PROMISCUOUS, None),
        (ReceiveFlags::empty(), None),
    ];
    let mut last_err = None;
    for (multicast, filters) in candidates {
        if !supported.contains(multicast) {
            log::debug!("net: receive filter {multicast:?} not supported, skipped");
            continue;
        }
        let enable = base | multicast;
        match snp.receive_filters(enable, supported - enable, false, filters) {
            Ok(()) if multicast.is_empty() => {
                log::warn!(
                    "net: no multicast receive mode accepted, mDNS works only if the NIC passes multicast anyway"
                );
                return Ok(());
            },
            Ok(()) => {
                log::debug!("net: receive filters set to {enable:?}");
                return Ok(());
            },
            Err(e) => {
                log::debug!("net: receive filters {enable:?} refused with {:?}", e.status());
                last_err = Some(e);
            },
        }
    }
    let source = last_err.unwrap_or_else(|| uefi::Error::from(Status::UNSUPPORTED));
    Err(NetError::Filter { handle, supported, source })
}

/// smoltcp's seed from the firmware RNG, else the TSC.
fn random_seed() -> u64 {
    rng_seed().unwrap_or_else(|| {
        log::debug!("net: no RNG protocol, seeding from the TSC");
        tsc_seed()
    })
}

#[expect(clippy::disallowed_methods, reason = "the raw TSC is used as entropy, not as a time source")]
fn tsc_seed() -> u64 {
    // SAFETY: RDTSC has no preconditions on x86_64.
    unsafe { core::arch::x86_64::_rdtsc() }
}

fn rng_seed() -> Option<u64> {
    let handle = boot::get_handle_for_protocol::<Rng>().ok()?;
    let mut rng = boot::open_protocol_exclusive::<Rng>(handle).ok()?;
    let mut seed = [0; 8];
    rng.get_rng(None, &mut seed).ok()?;
    Some(u64::from_ne_bytes(seed))
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    #[test]
    fn group_mac_keeps_the_low_23_bits() {
        check!(ipv4_group_mac(MDNS_ADDR).0[..6] == [0x01, 0x00, 0x5E, 0x00, 0x00, 0xFB]);
        check!(ipv4_group_mac(Ipv4Address::new(239, 255, 255, 250)).0[..6] == [0x01, 0x00, 0x5E, 0x7F, 0xFF, 0xFA]);
    }
}
