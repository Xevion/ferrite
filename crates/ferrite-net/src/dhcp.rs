//! smoltcp's `DHCPv4` client, with each lease applied to the interface (address and default route) and
//! logged.

use alloc::vec::Vec;

use smoltcp::iface::{Interface, SocketHandle, SocketSet};
use smoltcp::socket::dhcpv4;
use smoltcp::wire::{IpCidr, Ipv4Address, Ipv4Cidr};

/// What a DHCP server handed out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    pub address: Ipv4Cidr,
    pub router: Option<Ipv4Address>,
    pub dns: Vec<Ipv4Address>,
}

pub struct DhcpClient {
    handle: SocketHandle,
    lease: Option<Lease>,
}

impl DhcpClient {
    pub fn new(sockets: &mut SocketSet<'static>) -> Self {
        Self { handle: sockets.add(dhcpv4::Socket::new()), lease: None }
    }

    /// Apply a new lease or its loss to `iface`; returns the lease when it changed to one.
    pub fn poll(&mut self, iface: &mut Interface, sockets: &mut SocketSet<'static>) -> Option<&Lease> {
        let lease = match sockets.get_mut::<dhcpv4::Socket<'_>>(self.handle).poll()? {
            dhcpv4::Event::Configured(config) => Some(Lease {
                address: config.address,
                router: config.router,
                dns: config.dns_servers.iter().copied().collect(),
            }),
            dhcpv4::Event::Deconfigured if self.lease.is_none() => return None,
            dhcpv4::Event::Deconfigured => None,
        };
        iface.update_ip_addrs(|addrs| {
            addrs.clear();
            if let Some(lease) = &lease {
                let _ = addrs.push(IpCidr::Ipv4(lease.address));
            }
        });
        match lease.as_ref().and_then(|lease| lease.router) {
            Some(router) => {
                if let Err(error) = iface.routes_mut().add_default_ipv4_route(router) {
                    log::warn!("dhcp: adding the default route via {router} failed: {error:?}");
                }
            },
            None => {
                iface.routes_mut().remove_default_ipv4_route();
            },
        }
        match &lease {
            Some(lease) => log::info!(
                "dhcp: lease {} (prefix /{}), router {}, DNS {:?}",
                lease.address.address(),
                lease.address.prefix_len(),
                lease.router.map_or_else(|| alloc::string::String::from("none"), |r| alloc::format!("{r}")),
                lease.dns
            ),
            None => log::warn!("dhcp: the lease was lost"),
        }
        self.lease = lease;
        self.lease.as_ref()
    }

    pub const fn lease(&self) -> Option<&Lease> {
        self.lease.as_ref()
    }
}
