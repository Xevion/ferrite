//! A small HTTP/1.0 server for the status page.
//!
//! Each listening socket serves one request and closes. Requests are read up
//! to the blank line ending the headers; only `GET /` returns the page.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::time::Duration;

use smoltcp::iface::{SocketHandle, SocketSet};
use smoltcp::socket::tcp;

use super::NetStatus;
use crate::sweep::SweepStatus;

const HTTP_PORT: u16 = 80;
/// Listening sockets, so a browser's second parallel connection is not left waiting.
pub const HTTP_SOCKETS: usize = 2;
const REQUEST_CAP: usize = 2048;
const RX_BUFFER: usize = 2048;
const TX_BUFFER: usize = 4096;
/// A connection still open this long after it was accepted is aborted.
const CONNECTION_DEADLINE: Duration = Duration::from_secs(10);
const TCP_TIMEOUT: smoltcp::time::Duration = smoltcp::time::Duration::from_secs(5);
const TCP_KEEP_ALIVE: smoltcp::time::Duration = smoltcp::time::Duration::from_secs(2);

/// What a complete request asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    StatusPage,
    NotFound,
    MethodNotAllowed,
    BadRequest,
    HeadersTooLarge,
}

/// Route the request in `req`, or `None` while its headers are incomplete.
pub fn route(req: &[u8]) -> Option<Route> {
    let Some(head_end) = req.windows(4).position(|w| w == b"\r\n\r\n") else {
        return (req.len() >= REQUEST_CAP).then_some(Route::HeadersTooLarge);
    };
    let line_end = req.windows(2).position(|w| w == b"\r\n").unwrap_or(head_end);
    let mut parts = req[..line_end].split(|&b| b == b' ');
    let (Some(method), Some(target), Some(version), None) = (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Some(Route::BadRequest);
    };
    if !version.starts_with(b"HTTP/") || !target.starts_with(b"/") {
        return Some(Route::BadRequest);
    }
    if method != b"GET" {
        return Some(Route::MethodNotAllowed);
    }
    let path = target.split(|&b| b == b'?').next().unwrap_or(target);
    Some(match path {
        b"/" | b"/index.html" => Route::StatusPage,
        _ => Route::NotFound,
    })
}

/// The full response for `route`; the page is built only when it is asked for.
pub fn response(route: Route, page: impl FnOnce() -> String) -> Vec<u8> {
    let (status, extra, body) = match route {
        Route::StatusPage => ("200 OK", "", page()),
        Route::NotFound => ("404 Not Found", "", String::from("not found\n")),
        Route::MethodNotAllowed => ("405 Method Not Allowed", "Allow: GET\r\n", String::from("only GET\n")),
        Route::BadRequest => ("400 Bad Request", "", String::from("bad request\n")),
        Route::HeadersTooLarge => ("431 Request Header Fields Too Large", "", String::from("headers too large\n")),
    };
    let content_type = if route == Route::StatusPage { "text/html; charset=utf-8" } else { "text/plain" };
    let head = format!(
        "HTTP/1.0 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n",
        body.len()
    );
    let mut out = Vec::with_capacity(head.len() + body.len());
    out.extend_from_slice(head.as_bytes());
    out.extend_from_slice(body.as_bytes());
    out
}

pub fn status_page(net: &NetStatus, sweep: &SweepStatus) -> String {
    let link = &net.link;
    let ip = link.address.map_or_else(|| String::from("(none)"), |c| format!("{c}"));
    let verdict = sweep.first_fault.map_or_else(
        || String::from("no faults"),
        |f| {
            format!(
                "{} FAULTS, first at {}: expected {:#018x}, read {:#018x}",
                sweep.faults, f.addr, f.expected, f.actual
            )
        },
    );
    let c = &net.counters;
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <title>ferrite-uefi</title></head><body style=\"font-family:monospace\">\
         <h1>ferrite-uefi</h1>\
         <p>bare-metal memory tester (spike), no OS underneath</p>\
         <ul><li>ip: {ip}</li><li>mac: {}</li>\
         <li>sweep: {} MiB, pattern {:#04x} {}, {}%</li>\
         <li>pass {}, {verdict}</li>\
         <li>mdns queries: {}, http responses: {}</li></ul>\
         </body></html>",
        link.mac,
        sweep.bytes >> 20,
        sweep.pattern,
        sweep.phase,
        sweep.percent,
        sweep.passes,
        c.mdns_queries,
        c.http_responses,
    )
}

enum Stage {
    Reading,
    Sending { response: Vec<u8>, sent: usize },
    Closing,
}

struct Conn {
    handle: SocketHandle,
    request: Vec<u8>,
    stage: Stage,
    accepted: Option<Duration>,
}

impl Conn {
    fn reset(&mut self) {
        self.request.clear();
        self.stage = Stage::Reading;
        self.accepted = None;
    }
}

pub struct HttpServer {
    conns: Vec<Conn>,
    listen_failed: bool,
}

impl HttpServer {
    pub fn new(sockets: &mut SocketSet<'static>) -> Self {
        let conns = (0..HTTP_SOCKETS)
            .map(|_| {
                let mut sock = tcp::Socket::new(
                    tcp::SocketBuffer::new(vec![0; RX_BUFFER]),
                    tcp::SocketBuffer::new(vec![0; TX_BUFFER]),
                );
                sock.set_timeout(Some(TCP_TIMEOUT));
                sock.set_keep_alive(Some(TCP_KEEP_ALIVE));
                Conn {
                    handle: sockets.add(sock),
                    request: Vec::with_capacity(REQUEST_CAP),
                    stage: Stage::Reading,
                    accepted: None,
                }
            })
            .collect();
        Self { conns, listen_failed: false }
    }

    /// Advance every connection. Returns how many responses were completed.
    pub fn poll(&mut self, sockets: &mut SocketSet<'_>, now: Duration, mut page: impl FnMut() -> String) -> u32 {
        let mut completed = 0;
        for conn in &mut self.conns {
            let sock = sockets.get_mut::<tcp::Socket<'_>>(conn.handle);
            if !sock.is_open() {
                conn.reset();
                if let Err(e) = sock.listen(HTTP_PORT)
                    && !self.listen_failed
                {
                    self.listen_failed = true;
                    log::warn!("http: listen on :{HTTP_PORT} failed: {e:?}");
                }
                continue;
            }
            if sock.state() == tcp::State::Listen {
                conn.reset();
                continue;
            }
            let accepted = *conn.accepted.get_or_insert(now);
            if now.saturating_sub(accepted) >= CONNECTION_DEADLINE {
                log::debug!("http: aborting connection from {:?} open {CONNECTION_DEADLINE:?}", sock.remote_endpoint());
                sock.abort();
                conn.reset();
                continue;
            }
            if advance(conn, sock, &mut page) {
                completed += 1;
            }
        }
        completed
    }
}

/// Move one accepted connection along. Returns whether its response just finished sending.
fn advance(conn: &mut Conn, sock: &mut tcp::Socket<'_>, page: &mut impl FnMut() -> String) -> bool {
    if matches!(conn.stage, Stage::Reading) {
        if sock.can_recv() {
            let room = REQUEST_CAP - conn.request.len();
            let request = &mut conn.request;
            let _ = sock.recv(|buf| {
                let take = buf.len().min(room);
                request.extend_from_slice(&buf[..take]);
                (take, ())
            });
        }
        match route(&conn.request) {
            Some(route) => conn.stage = Stage::Sending { response: response(route, &mut *page), sent: 0 },
            // The client sent FIN without a complete request.
            None if sock.state() == tcp::State::CloseWait => {
                sock.close();
                conn.stage = Stage::Closing;
            },
            None => {},
        }
    }
    let Stage::Sending { response, sent } = &mut conn.stage else {
        return false;
    };
    if sock.can_send() {
        match sock.send_slice(&response[*sent..]) {
            Ok(n) => *sent += n,
            Err(e) => {
                log::debug!("http: send failed: {e:?}");
                sock.abort();
                conn.stage = Stage::Closing;
                return false;
            },
        }
    }
    if *sent < response.len() {
        return false;
    }
    sock.close();
    conn.stage = Stage::Closing;
    true
}

#[cfg(test)]
mod tests {
    use assert2::check;
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::root(b"GET / HTTP/1.1\r\nHost: ferrite.local\r\n\r\n", Some(Route::StatusPage))]
    #[case::index_with_query(b"GET /index.html?x=1 HTTP/1.0\r\n\r\n", Some(Route::StatusPage))]
    #[case::favicon(b"GET /favicon.ico HTTP/1.1\r\n\r\n", Some(Route::NotFound))]
    #[case::unknown_path(b"GET /admin HTTP/1.1\r\n\r\n", Some(Route::NotFound))]
    #[case::post(b"POST / HTTP/1.1\r\nContent-Length: 0\r\n\r\n", Some(Route::MethodNotAllowed))]
    #[case::headers_incomplete(b"GET / HTTP/1.1\r\nHost: x\r\n", None)]
    #[case::missing_version(b"GET /\r\n\r\n", Some(Route::BadRequest))]
    #[case::not_http(b"GET / FTP/1.0\r\n\r\n", Some(Route::BadRequest))]
    #[case::relative_target(b"GET index HTTP/1.0\r\n\r\n", Some(Route::BadRequest))]
    fn routes(#[case] req: &[u8], #[case] expected: Option<Route>) {
        check!(route(req) == expected);
    }

    #[test]
    fn oversized_headers_are_rejected_once_the_buffer_is_full() {
        let mut req = b"GET / HTTP/1.1\r\nX: ".to_vec();
        req.resize(REQUEST_CAP - 1, b'a');
        check!(route(&req).is_none());
        req.push(b'a');
        check!(route(&req) == Some(Route::HeadersTooLarge));
    }

    #[test]
    fn page_is_built_only_for_the_status_route() {
        let not_found = response(Route::NotFound, || unreachable!("page built for a 404"));
        check!(not_found.starts_with(b"HTTP/1.0 404 Not Found\r\n"));

        let ok = String::from_utf8(response(Route::StatusPage, || String::from("<p>hi</p>"))).unwrap();
        check!(
            ok == "HTTP/1.0 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: 9\r\nConnection: close\r\n\r\n<p>hi</p>"
        );
    }

    /// A smoltcp client on the same loopback interface; returns the bytes it read and responses served.
    fn exchange(request: &[u8]) -> (Vec<u8>, u32) {
        use smoltcp::iface::{Config, Interface};
        use smoltcp::phy::{Loopback, Medium};
        use smoltcp::time::Instant;
        use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr};

        let mut device = Loopback::new(Medium::Ethernet);
        let config = Config::new(HardwareAddress::Ethernet(EthernetAddress([0x02, 0, 0, 0, 0, 1])));
        let mut iface = Interface::new(config, &mut device, Instant::ZERO);
        iface.update_ip_addrs(|addrs| addrs.push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8)).unwrap());
        let mut sockets = SocketSet::new(vec![]);
        let mut http = HttpServer::new(&mut sockets);
        let client =
            sockets.add(tcp::Socket::new(tcp::SocketBuffer::new(vec![0; 4096]), tcp::SocketBuffer::new(vec![0; 4096])));

        let (mut reply, mut served, mut sent) = (Vec::new(), 0, false);
        for ms in 0..500_u32 {
            iface.poll(Instant::from_millis(ms), &mut device, &mut sockets);
            served += http.poll(&mut sockets, Duration::from_millis(ms.into()), || String::from("<p>page</p>"));
            let sock = sockets.get_mut::<tcp::Socket<'_>>(client);
            if ms == 1 {
                sock.connect(iface.context(), (IpAddress::v4(127, 0, 0, 1), HTTP_PORT), 49_152).unwrap();
            }
            if !sent && sock.may_send() {
                sent = sock.send_slice(request).unwrap() == request.len();
            }
            if sock.can_recv() {
                sock.recv(|buf| {
                    reply.extend_from_slice(buf);
                    (buf.len(), ())
                })
                .unwrap();
            }
        }
        (reply, served)
    }

    #[rstest]
    #[case::get(b"GET / HTTP/1.1\r\nHost: ferrite.local\r\n\r\n", "HTTP/1.0 200 OK\r\n")]
    #[case::favicon(b"GET /favicon.ico HTTP/1.1\r\n\r\n", "HTTP/1.0 404 Not Found\r\n")]
    #[case::post(b"POST / HTTP/1.1\r\nContent-Length: 0\r\n\r\n", "HTTP/1.0 405 Method Not Allowed\r\n")]
    fn serves_over_tcp(#[case] request: &[u8], #[case] status_line: &str) {
        let (reply, served) = exchange(request);
        check!(served == 1);
        check!(String::from_utf8_lossy(&reply).starts_with(status_line));
    }
}
