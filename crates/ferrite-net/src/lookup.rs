//! A host name looked up with a DNS server through one of the stack's sockets, polled with the stack
//! and never blocking. Shared by everything that connects to a configured host name.

use alloc::vec;

use smoltcp::iface::{Interface, SocketHandle, SocketSet};
use smoltcp::socket::dns::{self, GetQueryResultError, QueryHandle};
use smoltcp::wire::{DnsQueryType, IpAddress, Ipv4Address};

use crate::notify::HostName;

/// Why a lookup could not start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// The lease names no DNS server.
    NoServer,
    /// The stack would not take the name for a query.
    BadName,
}

/// How a lookup stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    Pending,
    Found(Ipv4Address),
    /// The server said no, did not answer in time, or gave no IPv4 address.
    Failed,
}

/// A query in flight, and the socket it runs on until [`Lookup::release`].
#[derive(Clone, Copy)]
pub struct Lookup {
    socket: SocketHandle,
    query: QueryHandle,
}

impl Lookup {
    /// Ask `server` for the IPv4 address of `name`.
    pub fn start(
        iface: &mut Interface,
        sockets: &mut SocketSet<'static>,
        server: Option<Ipv4Address>,
        name: &HostName,
    ) -> Result<Self, Refused> {
        let server = server.ok_or(Refused::NoServer)?;
        let mut socket = dns::Socket::new(&[IpAddress::Ipv4(server)], vec![None]);
        let query =
            socket.start_query(iface.context(), name.as_str(), DnsQueryType::A).map_err(|_| Refused::BadName)?;
        Ok(Self { socket: sockets.add(socket), query })
    }

    pub fn poll(&self, sockets: &mut SocketSet<'static>) -> Progress {
        match sockets.get_mut::<dns::Socket<'_>>(self.socket).get_query_result(self.query) {
            Ok(addresses) => match addresses.first() {
                Some(&IpAddress::Ipv4(ip)) => Progress::Found(ip),
                _ => Progress::Failed,
            },
            Err(GetQueryResultError::Pending) => Progress::Pending,
            Err(GetQueryResultError::Failed) => Progress::Failed,
        }
    }

    /// Give the socket back.
    pub fn release(self, sockets: &mut SocketSet<'static>) {
        sockets.remove(self.socket);
    }
}
