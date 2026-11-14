//! The IP stack of one port: a smoltcp interface and its sockets, a DHCP client or a fixed address,
//! the mDNS responder, ICMP echo (answered by smoltcp, sent by [`Stack::start_ping`]), the HTTP
//! server and the sockets of a notification being sent.
//!
//! The stack owns its [`Port`] and moves frames through fixed buffers. Several stacks side by side
//! (one per port) make a [`crate::Network`].

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::time::Duration;

use ferrite_hal::{Clock, Instant};
use smoltcp::iface::{Config, Interface, PollResult, SocketHandle, SocketSet};
use smoltcp::socket::{dhcpv4, udp};
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address, Ipv4Cidr};

use crate::device::{PortDevice, Scratch, smol_instant};
use crate::dhcp;
use crate::http::{PollStats, Server, Site, Web};
use crate::mdns::{self, Destination, Handled, Responder};
use crate::notify::{Courier, Notifier};
use crate::ping::Pinger;
use crate::port::Port;
use crate::sntp::{Prober, Sync};
use crate::status::{Link, NtpServers, OptAddr, PortStatus, ServiceCounters};

const MDNS_BUFFER: usize = 1500;
const MDNS_PACKETS: usize = 4;

/// Longest gap between polls when smoltcp has no timer due sooner.
const IDLE_POLL: Duration = Duration::from_millis(5);
/// Gap between polls while frames are arriving or sockets are changing.
const BUSY_POLL: Duration = Duration::from_millis(1);

/// What a stack runs, and how it gets its address.
#[derive(Clone, Copy, Debug)]
pub struct StackConfig {
    /// smoltcp's random seed (initial sequence numbers, DHCP transaction ids).
    pub seed: u64,
    /// This address and no DHCP client, else a DHCP client.
    pub address: Option<Ipv4Cidr>,
    /// Answer mDNS queries for the host name with the stack's own address.
    pub mdns: bool,
    /// Serve the network's site on port 80.
    pub http: bool,
}

impl StackConfig {
    /// DHCP, mDNS and HTTP: what an appliance's wired port runs.
    #[must_use]
    pub const fn appliance(seed: u64) -> Self {
        Self { seed, address: None, mdns: true, http: true }
    }
}

/// What a poll takes besides the time.
pub struct PollCtx<'a> {
    /// Writes the site's document when a connection needs a current one.
    pub render: &'a mut dyn FnMut(&mut String),
    /// Times the HTTP poll, for the periodic summary; without one the timings read zero.
    pub clock: Option<&'a dyn Clock>,
    /// The notifier whose queue this stack carries, when it is the port notifications go out on.
    pub notify: Option<&'a mut Notifier>,
    /// The time client whose requests this stack carries, when it is the port they go out on.
    pub sntp: Option<&'a mut Sync>,
}

struct Mdns {
    socket: SocketHandle,
    responder: Responder,
    out: Vec<u8>,
}

pub struct Stack {
    port: Box<dyn Port>,
    scratch: Box<Scratch>,
    iface: Interface,
    sockets: SocketSet<'static>,
    dhcp: Option<SocketHandle>,
    mdns: Option<Mdns>,
    http: Option<Server>,
    pinger: Option<Pinger>,
    courier: Courier,
    prober: Prober,
    link: Link,
    services: ServiceCounters,
    http_stats: PollStats,
}

impl Stack {
    /// A stack on `port` as of `now`. Its HTTP server, if `config` asks for one, starts with [`Stack::serve`].
    pub fn new(port: Box<dyn Port>, config: StackConfig, now: Instant) -> Self {
        let mut port = port;
        let mut scratch = Scratch::new();
        let mac = port.mac();
        let mut iface_config = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac.octets())));
        iface_config.random_seed = config.seed;
        let mut iface =
            Interface::new(iface_config, &mut PortDevice::new(&mut *port, &mut scratch, now), smol_instant(now));

        let mut sockets = SocketSet::new(vec![]);
        let mut link = Link { medium: port.medium(), up: port.link_up(), mac, ..Link::default() };
        if let Some(cidr) = config.address {
            iface.update_ip_addrs(|addrs| {
                let _ = addrs.push(IpCidr::Ipv4(cidr));
            });
            link.address = Some(cidr);
        }
        let dhcp = config.address.is_none().then(|| sockets.add(dhcp_socket()));
        let mdns = config.mdns.then(|| Mdns::new(&mut iface, &mut sockets));
        Self {
            port,
            scratch,
            iface,
            sockets,
            dhcp,
            mdns,
            http: None,
            pinger: None,
            courier: Courier::new(config.seed),
            prober: Prober::default(),
            link,
            services: ServiceCounters::default(),
            http_stats: PollStats::default(),
        }
    }

    /// Serve `web`'s site on port 80.
    pub fn serve<S: Site>(&mut self, web: &Rc<Web<S>>) {
        if self.http.is_none() {
            self.http = Some(Server::new(web, &mut self.sockets));
        }
    }

    /// Advance the stack to `now`. Returns how long may pass before the next poll.
    pub fn poll(&mut self, now: Instant, ctx: &mut PollCtx<'_>) -> Duration {
        self.port.service(now);
        self.link.up = self.port.link_up();
        let at = smol_instant(now);

        let rx_before = self.port.stats().rx_frames;
        let mut device = PortDevice::new(&mut *self.port, &mut self.scratch, now);
        let changed = self.iface.poll(at, &mut device, &mut self.sockets) == PollResult::SocketStateChanged;
        self.poll_dhcp(now);
        if let Some(pinger) = &mut self.pinger {
            pinger.poll(&mut self.sockets, now);
        }

        let busy = self.http.as_mut().is_some_and(|server| {
            let started = ctx.clock.map(Clock::now);
            let served = server.poll(&mut self.sockets, now, &mut *ctx.render);
            let took = ctx.clock.zip(started).map_or(Duration::ZERO, |(clock, started)| clock.now() - started);
            self.http_stats.record(now, took, &served);
            self.services.http_responses = self.services.http_responses.wrapping_add(served.responses);
            served.busy
        });
        self.courier.poll(&mut self.iface, &mut self.sockets, &self.link, ctx.notify.as_deref_mut(), now);
        self.prober.poll(&mut self.iface, &mut self.sockets, &self.link, ctx.sntp.as_deref_mut(), now);
        // Send what the connections just queued now rather than a poll later.
        let mut device = PortDevice::new(&mut *self.port, &mut self.scratch, now);
        self.iface.poll_egress(at, &mut device, &mut self.sockets);
        self.serve_mdns(now);

        if changed || busy || self.port.stats().rx_frames != rx_before {
            return BUSY_POLL;
        }
        self.iface.poll_delay(at, &self.sockets).map_or(IDLE_POLL, |d| Duration::from(d).min(IDLE_POLL))
    }

    fn poll_dhcp(&mut self, now: Instant) {
        let Some(handle) = self.dhcp else { return };
        // The event borrows the socket, so copy out what is needed first.
        let lease = match self.sockets.get_mut::<dhcpv4::Socket<'_>>(handle).poll() {
            Some(dhcpv4::Event::Configured(c)) => Some(Some(Lease {
                address: c.address,
                router: c.router,
                dns: c.dns_servers.first().copied(),
                ntp: c.packet.map_or_else(NtpServers::default, |packet| dhcp::ntp_servers(packet.into_inner())),
            })),
            Some(dhcpv4::Event::Deconfigured) => Some(None),
            None => None,
        };
        match lease {
            // The client reports every renewal, since it keeps the reply; only a change is news.
            Some(Some(lease)) if lease.is_in_force(&self.link) => {},
            Some(Some(lease)) => self.apply_lease(now, &lease),
            Some(None) => self.clear_lease(),
            None => {},
        }
    }

    fn apply_lease(&mut self, now: Instant, lease: &Lease) {
        let Lease { address, router, dns, ntp } = *lease;
        self.iface.update_ip_addrs(|addrs| {
            addrs.clear();
            // The list was just emptied and holds at least one address, so this push cannot fail.
            let _ = addrs.push(IpCidr::Ipv4(address));
        });
        match router {
            Some(router) => {
                if let Err(e) = self.iface.routes_mut().add_default_ipv4_route(router) {
                    log::warn!("net: {}: adding default route via {router} failed: {e:?}", self.link.mac);
                }
            },
            None => {
                self.iface.routes_mut().remove_default_ipv4_route();
            },
        }
        log::info!(
            "dhcp: lease {} (prefix /{}), router {}, DNS {}, NTP {ntp}",
            address.address(),
            address.prefix_len(),
            OptAddr(router),
            OptAddr(dns)
        );
        if self.link.address != Some(address)
            && let Some(mdns) = &mut self.mdns
        {
            mdns.responder.schedule_announcements(now);
        }
        self.link.address = Some(address);
        self.link.gateway = router;
        self.link.dns = dns;
        self.link.ntp = ntp;
    }

    fn clear_lease(&mut self) {
        #[expect(
            clippy::redundant_closure_for_method_calls,
            reason = "the method's crate, heapless, is not a dependency here"
        )]
        self.iface.update_ip_addrs(|addrs| addrs.clear());
        self.iface.routes_mut().remove_default_ipv4_route();
        self.link.address = None;
        self.link.gateway = None;
        self.link.dns = None;
        self.link.ntp = NtpServers::default();
    }

    /// Answer queued mDNS queries and send any due announcement.
    fn serve_mdns(&mut self, now: Instant) {
        let Some(mdns) = &mut self.mdns else { return };
        let ip = self.link.address.map(|cidr| cidr.address());
        let group = IpEndpoint::new(IpAddress::Ipv4(mdns::MDNS_GROUP), mdns::MDNS_PORT);
        let sock = self.sockets.get_mut::<udp::Socket<'_>>(mdns.socket);
        let counters = &mut self.services;
        let out = &mut mdns.out;

        if let Some(ip) = ip
            && mdns.responder.announcement(now, ip, out)
        {
            send_mdns(sock, out, group, counters);
        }
        while let Ok((data, meta)) = sock.recv() {
            let source = meta.endpoint;
            match mdns.responder.handle(data, source.port, now, ip, out) {
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

    /// Send `count` echo requests to `target`, `interval` apart, the first at once; each waits at
    /// most `timeout` for its reply. Replaces a ping in progress.
    pub fn start_ping(
        &mut self,
        target: Ipv4Address,
        ident: u16,
        count: u16,
        interval: Duration,
        timeout: Duration,
        now: Instant,
    ) {
        if let Some(old) = self.pinger.take() {
            old.remove(&mut self.sockets);
        }
        self.pinger = Some(Pinger::new(&mut self.sockets, target, ident, count, interval, timeout, now));
    }

    pub const fn pinger(&self) -> Option<&Pinger> {
        self.pinger.as_ref()
    }

    /// Whether a notification is being sent over this stack.
    pub const fn notifying(&self) -> bool {
        self.courier.busy()
    }

    /// Whether a time request is being sent over this stack.
    pub const fn syncing(&self) -> bool {
        self.prober.busy()
    }

    /// The DNS server to look names up with, for a stack whose address is fixed (a DHCP lease sets it).
    pub const fn set_dns(&mut self, server: Option<Ipv4Address>) {
        self.link.dns = server;
    }

    /// Link and lease state.
    pub const fn link(&self) -> &Link {
        &self.link
    }

    pub fn status(&self) -> PortStatus {
        PortStatus { link: self.link, frames: self.port.stats(), services: self.services }
    }

    pub fn port(&self) -> &dyn Port {
        &*self.port
    }

    /// The interface and its sockets, for an app that runs sockets of its own beside the services.
    pub fn parts(&mut self) -> (&mut Interface, &mut SocketSet<'static>) {
        (&mut self.iface, &mut self.sockets)
    }

    /// Whether the interface wants polling again at once.
    pub fn poll_delay(&mut self, now: Instant) -> Option<Duration> {
        self.iface.poll_delay(smol_instant(now), &self.sockets).map(Duration::from)
    }
}

/// What a DHCP lease configures.
#[derive(Clone, Copy)]
struct Lease {
    address: Ipv4Cidr,
    router: Option<Ipv4Address>,
    dns: Option<Ipv4Address>,
    ntp: NtpServers,
}

impl Lease {
    fn is_in_force(&self, link: &Link) -> bool {
        link.address == Some(self.address)
            && link.gateway == self.router
            && link.dns == self.dns
            && link.ntp == self.ntp
    }
}

/// A DHCP client that also asks for the NTP servers and keeps the reply to read them from.
fn dhcp_socket() -> dhcpv4::Socket<'static> {
    let mut socket = dhcpv4::Socket::new();
    socket.set_parameter_request_list(&dhcp::PARAMETER_REQUEST_LIST);
    socket.set_receive_packet_buffer(dhcp::reply_buffer());
    socket
}

impl Mdns {
    fn new(iface: &mut Interface, sockets: &mut SocketSet<'static>) -> Self {
        if let Err(e) = iface.join_multicast_group(mdns::MDNS_GROUP) {
            log::warn!("net: joining {} failed, mDNS disabled: {e}", mdns::MDNS_GROUP);
        }
        let mut socket = udp::Socket::new(
            udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; MDNS_PACKETS], vec![0; MDNS_BUFFER]),
            udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; MDNS_PACKETS], vec![0; MDNS_BUFFER]),
        );
        if let Err(e) = socket.bind(mdns::MDNS_PORT) {
            log::warn!("net: binding udp :{} failed, mDNS disabled: {e}", mdns::MDNS_PORT);
        }
        Self { socket: sockets.add(socket), responder: Responder::new(), out: Vec::with_capacity(MDNS_BUFFER) }
    }
}

fn send_mdns(sock: &mut udp::Socket<'_>, out: &[u8], to: IpEndpoint, counters: &mut ServiceCounters) {
    match sock.send_slice(out, to) {
        Ok(()) => counters.mdns_replies = counters.mdns_replies.wrapping_add(1),
        Err(e) => log::debug!("mdns: reply to {to} dropped: {e}"),
    }
}
