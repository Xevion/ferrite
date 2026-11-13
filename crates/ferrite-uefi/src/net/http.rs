//! A small HTTP/1.0 server for the status page.
//!
//! Each listening socket serves one request and closes. Requests are read up
//! to the blank line ending the headers; only `GET /` returns the page.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::time::Duration;

use ferrite_hal::Instant;
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
                "{} FAULTS, first at {} in {}: expected {:#018x}, read {:#018x}; {}",
                sweep.faults, f.addr, sweep.first_fault_test, f.expected, f.actual, sweep.analysis
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
         <li>sweep: {} MiB ({}), test {} ({}/{}) {}%, pass {}%</li>\
         <li>pass {}, {verdict}</li>\
         <li>mdns queries: {}, http responses: {}</li></ul>\
         </body></html>",
        link.mac,
        sweep.bytes >> 20,
        sweep.tier,
        sweep.test,
        sweep.position.0,
        sweep.position.1,
        sweep.test_percent,
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
    accepted: Option<Instant>,
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
    pub fn poll(&mut self, sockets: &mut SocketSet<'_>, now: Instant, mut page: impl FnMut() -> String) -> u32 {
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
            if now - accepted >= CONNECTION_DEADLINE {
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
            let received = sock.recv(|buf| {
                let take = buf.len().min(room);
                request.extend_from_slice(&buf[..take]);
                (take, ())
            });
            if let Err(e) = received {
                log::debug!("http: receive failed: {e:?}");
            }
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

    /// The server and one client socket on a smoltcp loopback interface, stepped a millisecond at a time.
    struct Loopback {
        device: smoltcp::phy::Loopback,
        iface: smoltcp::iface::Interface,
        sockets: SocketSet<'static>,
        http: HttpServer,
        client: SocketHandle,
        ms: u64,
        served: u32,
        reply: Vec<u8>,
    }

    impl Loopback {
        fn new() -> Self {
            use smoltcp::iface::{Config, Interface};
            use smoltcp::phy::Medium;
            use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr};

            let mut device = smoltcp::phy::Loopback::new(Medium::Ethernet);
            let config = Config::new(HardwareAddress::Ethernet(EthernetAddress([0x02, 0, 0, 0, 0, 1])));
            let mut iface = Interface::new(config, &mut device, smoltcp::time::Instant::ZERO);
            iface.update_ip_addrs(|addrs| addrs.push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8)).unwrap());
            let mut sockets = SocketSet::new(vec![]);
            let http = HttpServer::new(&mut sockets);
            let client = sockets
                .add(tcp::Socket::new(tcp::SocketBuffer::new(vec![0; 4096]), tcp::SocketBuffer::new(vec![0; 4096])));
            Self { device, iface, sockets, http, client, ms: 0, served: 0, reply: Vec::new() }
        }

        fn client(&mut self) -> &mut tcp::Socket<'static> {
            self.sockets.get_mut(self.client)
        }

        fn connect(&mut self, local_port: u16) {
            self.reply.clear();
            let sock = self.sockets.get_mut::<tcp::Socket<'_>>(self.client);
            sock.connect(self.iface.context(), (smoltcp::wire::IpAddress::v4(127, 0, 0, 1), HTTP_PORT), local_port)
                .unwrap();
            self.run_until(tcp::Socket::may_send, 100);
        }

        /// Step until `done` holds for the client socket, at most `limit` milliseconds.
        fn run_until(&mut self, mut done: impl FnMut(&tcp::Socket<'static>) -> bool, limit: u64) {
            for _ in 0..limit {
                self.ms += 1;
                self.iface.poll(
                    smoltcp::time::Instant::from_millis(self.ms.cast_signed()),
                    &mut self.device,
                    &mut self.sockets,
                );
                let now = Instant::from_epoch(Duration::from_millis(self.ms));
                self.served += self.http.poll(&mut self.sockets, now, || String::from("<p>page</p>"));
                let sock = self.sockets.get_mut::<tcp::Socket<'static>>(self.client);
                if sock.can_recv() {
                    sock.recv(|buf| {
                        self.reply.extend_from_slice(buf);
                        (buf.len(), ())
                    })
                    .unwrap();
                }
                if done(sock) {
                    return;
                }
            }
        }

        fn run(&mut self, ms: u64) {
            self.run_until(|_| false, ms);
        }

        fn request(&mut self, request: &[u8]) {
            check!(self.client().send_slice(request).unwrap() == request.len());
        }
    }

    #[rstest]
    #[case::get(b"GET / HTTP/1.1\r\nHost: ferrite.local\r\n\r\n", "HTTP/1.0 200 OK\r\n")]
    #[case::favicon(b"GET /favicon.ico HTTP/1.1\r\n\r\n", "HTTP/1.0 404 Not Found\r\n")]
    #[case::post(b"POST / HTTP/1.1\r\nContent-Length: 0\r\n\r\n", "HTTP/1.0 405 Method Not Allowed\r\n")]
    fn serves_over_tcp(#[case] request: &[u8], #[case] status_line: &str) {
        let mut net = Loopback::new();
        net.connect(49_152);
        net.request(request);
        net.run(500);
        check!(net.served == 1);
        check!(String::from_utf8_lossy(&net.reply).starts_with(status_line));
    }

    #[test]
    fn a_client_closing_mid_request_gets_no_response() {
        let mut net = Loopback::new();
        net.connect(49_152);
        net.request(b"GET / HTTP/1.1\r\nHost: x\r\n");
        net.client().close();
        net.run(500);
        check!(net.served == 0);
        check!(net.reply.is_empty());
        check!(matches!(net.client().state(), tcp::State::TimeWait | tcp::State::Closed), "the server closed too");
    }

    #[test]
    fn an_idle_connection_is_aborted_at_the_deadline_and_the_socket_listens_again() {
        let mut net = Loopback::new();
        net.connect(49_152);
        let accepted = net.ms;
        net.run_until(|sock| !sock.is_open(), CONNECTION_DEADLINE.as_millis() as u64 + 100);
        check!(!net.client().is_open(), "reset by the server");
        check!(net.ms - accepted >= CONNECTION_DEADLINE.as_millis() as u64 - 10);
        check!(net.served == 0);

        net.connect(49_153);
        net.request(b"GET / HTTP/1.1\r\n\r\n");
        net.run(500);
        check!(net.served == 1);
    }
}
