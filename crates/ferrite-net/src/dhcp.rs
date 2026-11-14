//! What the DHCP client asks for beyond smoltcp's defaults, and what it reads of a reply that smoltcp
//! does not parse for it.

use alloc::boxed::Box;
use alloc::vec;

use smoltcp::wire::{DhcpPacket, Ipv4Address};

use crate::status::NtpServers;

const OPT_SUBNET_MASK: u8 = 1;
const OPT_ROUTER: u8 = 3;
const OPT_DOMAIN_NAME_SERVER: u8 = 6;
const OPT_NTP_SERVERS: u8 = 42;

/// smoltcp's own request (subnet mask, router, DNS servers) and the NTP servers.
pub const PARAMETER_REQUEST_LIST: [u8; 4] = [OPT_SUBNET_MASK, OPT_ROUTER, OPT_DOMAIN_NAME_SERVER, OPT_NTP_SERVERS];

/// What every client must be able to receive, which is all the options of a lease take.
const REPLY_BUFFER: usize = 576;

/// Where the client copies the last reply it received. The socket holds it for as long as it lives, and
/// a stack lives as long as its port, so this is a few hundred bytes given up for each port that comes.
pub fn reply_buffer() -> &'static mut [u8] {
    Box::leak(vec![0; REPLY_BUFFER].into_boxed_slice())
}

/// The servers of option 42 in a DHCP packet (as the client saw it), skipping unspecified addresses.
pub fn ntp_servers(packet: &[u8]) -> NtpServers {
    let mut servers = NtpServers::default();
    DhcpPacket::new_unchecked(packet)
        .options()
        .filter(|option| option.kind == OPT_NTP_SERVERS)
        .flat_map(|option| option.data.as_chunks::<4>().0)
        .map(|&octets| Ipv4Address::from(octets))
        .filter(|server| !server.is_unspecified())
        .for_each(|server| {
            servers.push(server);
        });
    servers
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use assert2::check;
    use heapless::Vec as FixedVec;
    use smoltcp::wire::{DhcpMessageType, DhcpOption, DhcpPacket, DhcpRepr, EthernetAddress};

    use super::*;

    /// An ACK as smoltcp writes one, with `option42` as the data of option 42 when there is any.
    fn ack(option42: Option<&[u8]>) -> Vec<u8> {
        let extra = option42.map(|data| DhcpOption { kind: OPT_NTP_SERVERS, data });
        let options: Vec<DhcpOption<'_>> = extra.into_iter().collect();
        let mut dns = FixedVec::new();
        dns.push(Ipv4Address::new(10, 0, 2, 3)).unwrap();
        let repr = DhcpRepr {
            message_type: DhcpMessageType::Ack,
            transaction_id: 0x3d1d,
            secs: 0,
            client_hardware_address: EthernetAddress([0x52, 0x54, 0, 0x12, 0x34, 0x56]),
            client_ip: Ipv4Address::UNSPECIFIED,
            your_ip: Ipv4Address::new(10, 0, 2, 15),
            server_ip: Ipv4Address::new(10, 0, 2, 2),
            router: Some(Ipv4Address::new(10, 0, 2, 2)),
            subnet_mask: Some(Ipv4Address::new(255, 255, 255, 0)),
            relay_agent_ip: Ipv4Address::UNSPECIFIED,
            broadcast: false,
            requested_ip: None,
            client_identifier: None,
            server_identifier: Some(Ipv4Address::new(10, 0, 2, 2)),
            parameter_request_list: None,
            dns_servers: Some(dns),
            max_size: None,
            lease_duration: Some(86_400),
            renew_duration: None,
            rebind_duration: None,
            additional_options: &options,
        };
        let mut bytes = vec![0; repr.buffer_len()];
        repr.emit(&mut DhcpPacket::new_unchecked(&mut bytes[..])).unwrap();
        bytes
    }

    #[test]
    fn the_servers_of_option_42_are_read_in_order() {
        let servers = ntp_servers(&ack(Some(&[192, 0, 2, 1, 10, 0, 0, 7])));
        check!(servers.iter().collect::<Vec<_>>() == [Ipv4Address::new(192, 0, 2, 1), Ipv4Address::new(10, 0, 0, 7)]);
    }

    #[test]
    fn a_lease_without_the_option_names_none() {
        check!(ntp_servers(&ack(None)).is_empty());
    }

    #[test]
    fn more_servers_than_the_list_holds_and_a_ragged_tail_are_cut() {
        let six: Vec<u8> = (1..=6).flat_map(|n| [10, 0, 0, n]).collect();
        check!(ntp_servers(&ack(Some(&six))).iter().count() == crate::status::MAX_NTP_SERVERS);
        check!(ntp_servers(&ack(Some(&[10, 0, 0, 1, 10, 0]))).iter().count() == 1);
    }

    #[test]
    fn the_zero_address_is_no_server() {
        check!(
            ntp_servers(&ack(Some(&[0, 0, 0, 0, 10, 0, 0, 1]))).iter().collect::<Vec<_>>()
                == [Ipv4Address::new(10, 0, 0, 1)]
        );
    }

    // The client against a server that answers by hand.

    use alloc::boxed::Box;
    use alloc::rc::Rc;
    use core::time::Duration;

    use ferrite_hal::Instant;
    use smoltcp::phy::ChecksumCapabilities;
    use smoltcp::wire::{
        EthernetFrame, EthernetProtocol, EthernetRepr, IpAddress, IpProtocol, Ipv4Cidr, Ipv4Packet, Ipv4Repr,
        UdpPacket, UdpRepr,
    };

    use crate::queue::QueuePort;
    use crate::stack::{PollCtx, Stack, StackConfig};

    const SERVER: Ipv4Address = Ipv4Address::new(10, 0, 2, 2);
    const SERVER_MAC: EthernetAddress = EthernetAddress([0x52, 0x55, 0x0a, 0, 2, 2]);
    const CLIENT_MAC: [u8; 6] = [0x52, 0x54, 0, 0x12, 0x34, 0x56];

    /// A client stack on DHCP and what it takes to answer it.
    struct Rig {
        stack: Stack,
        link: Rc<QueuePort>,
        /// Option 42's data in the lease the server gives.
        ntp: Vec<u8>,
        /// The lease, in seconds: short, so that the client renews within a few simulated seconds.
        lease: u32,
        ms: u64,
        answered: u32,
    }

    impl Rig {
        fn new(ntp: &[u8]) -> Self {
            let link = Rc::new(QueuePort::new(CLIENT_MAC, 1500, 16, 16));
            let config = StackConfig { seed: 9, address: None, mdns: false, http: false };
            let stack = Stack::new(Box::new(Rc::clone(&link)), config, Instant::default());
            Self { stack, link, ntp: ntp.to_vec(), lease: 4, ms: 0, answered: 0 }
        }

        fn step(&mut self) {
            self.ms += 1;
            let now = Instant::from_epoch(Duration::from_millis(self.ms));
            let mut render = |_: &mut alloc::string::String| {};
            self.stack.poll(now, &mut PollCtx { render: &mut render, clock: None, notify: None, sntp: None });
            while let Some(frame) = self.link.next_tx() {
                self.link.transmitted(true);
                if let Some(reply) = self.answer(&frame) {
                    self.link.deliver(&reply);
                }
            }
        }

        fn run(&mut self, ms: u64) {
            (0..ms).for_each(|_| self.step());
        }

        /// The server's reply to a frame the client sent, when it is a DHCP DISCOVER or REQUEST.
        fn answer(&mut self, frame: &[u8]) -> Option<Vec<u8>> {
            let eth = EthernetFrame::new_checked(frame).ok()?;
            let ip = Ipv4Packet::new_checked(eth.payload()).ok()?;
            let udp = UdpPacket::new_checked(ip.payload()).ok().filter(|udp| udp.dst_port() == 67)?;
            let request = DhcpPacket::new_checked(udp.payload()).ok()?;
            let reply = match DhcpRepr::parse(&request).ok()?.message_type {
                DhcpMessageType::Discover => DhcpMessageType::Offer,
                DhcpMessageType::Request => DhcpMessageType::Ack,
                _ => return None,
            };
            self.answered += 1;
            let mut dns = FixedVec::new();
            dns.push(Ipv4Address::new(10, 0, 2, 3)).unwrap();
            let option42 = DhcpOption { kind: OPT_NTP_SERVERS, data: &self.ntp };
            let options: Vec<DhcpOption<'_>> = if self.ntp.is_empty() { Vec::new() } else { vec![option42] };
            let repr = DhcpRepr {
                message_type: reply,
                transaction_id: request.transaction_id(),
                secs: 0,
                client_hardware_address: EthernetAddress(CLIENT_MAC),
                client_ip: Ipv4Address::UNSPECIFIED,
                your_ip: Ipv4Address::new(10, 0, 2, 15),
                server_ip: SERVER,
                router: Some(SERVER),
                subnet_mask: Some(Ipv4Address::new(255, 255, 255, 0)),
                relay_agent_ip: Ipv4Address::UNSPECIFIED,
                broadcast: false,
                requested_ip: None,
                client_identifier: None,
                server_identifier: Some(SERVER),
                parameter_request_list: None,
                dns_servers: Some(dns),
                max_size: None,
                lease_duration: Some(self.lease),
                renew_duration: None,
                rebind_duration: None,
                additional_options: &options,
            };
            Some(broadcast_frame(&repr))
        }

        fn ntp(&self) -> Vec<Ipv4Address> {
            self.stack.link().ntp.iter().collect()
        }
    }

    fn broadcast_frame(repr: &DhcpRepr<'_>) -> Vec<u8> {
        let caps = ChecksumCapabilities::default();
        let udp = UdpRepr { src_port: 67, dst_port: 68 };
        let ip = Ipv4Repr {
            src_addr: SERVER,
            dst_addr: Ipv4Address::BROADCAST,
            next_header: IpProtocol::Udp,
            payload_len: udp.header_len() + repr.buffer_len(),
            hop_limit: 64,
        };
        let eth = EthernetRepr {
            src_addr: SERVER_MAC,
            dst_addr: EthernetAddress::BROADCAST,
            ethertype: EthernetProtocol::Ipv4,
        };
        let mut bytes = vec![0; eth.buffer_len() + ip.buffer_len() + ip.payload_len];
        eth.emit(&mut EthernetFrame::new_unchecked(&mut bytes[..]));
        let mut packet = Ipv4Packet::new_unchecked(&mut bytes[eth.buffer_len()..]);
        ip.emit(&mut packet, &caps);
        let (src, dst) = (IpAddress::Ipv4(ip.src_addr), IpAddress::Ipv4(ip.dst_addr));
        let mut datagram = UdpPacket::new_unchecked(packet.payload_mut());
        let emit = |payload: &mut [u8]| repr.emit(&mut DhcpPacket::new_unchecked(payload)).unwrap();
        udp.emit(&mut datagram, &src, &dst, repr.buffer_len(), emit, &caps);
        bytes
    }

    #[test]
    fn the_ntp_servers_of_the_lease_reach_the_link_beside_the_router_and_dns() {
        let mut rig = Rig::new(&[192, 0, 2, 1, 10, 0, 2, 3]);
        check!(rig.stack.link().ntp.is_empty());
        rig.run(2_000);
        let link = rig.stack.link();
        check!(link.address == Some(Ipv4Cidr::new(Ipv4Address::new(10, 0, 2, 15), 24)));
        check!(link.gateway == Some(SERVER) && link.dns == Some(Ipv4Address::new(10, 0, 2, 3)));
        check!(rig.ntp() == [Ipv4Address::new(192, 0, 2, 1), Ipv4Address::new(10, 0, 2, 3)]);
    }

    #[test]
    fn a_lease_without_the_option_leaves_the_list_empty() {
        let mut rig = Rig::new(&[]);
        rig.run(2_000);
        check!(rig.stack.link().address.is_some());
        check!(rig.stack.link().ntp.is_empty());
    }

    #[test]
    fn a_renewal_keeps_the_servers_and_a_changed_list_replaces_them() {
        let mut rig = Rig::new(&[10, 0, 0, 1]);
        rig.run(2_000);
        check!(rig.ntp() == [Ipv4Address::new(10, 0, 0, 1)]);
        let answered = rig.answered;

        rig.run(4_000);
        check!(rig.answered > answered, "the client renewed");
        check!(rig.ntp() == [Ipv4Address::new(10, 0, 0, 1)]);

        rig.ntp = vec![10, 0, 0, 9];
        rig.run(6_000);
        check!(rig.ntp() == [Ipv4Address::new(10, 0, 0, 9)]);
        check!(rig.stack.link().address.is_some());
    }
}
