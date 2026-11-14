//! The status page, its JSON API and the run's controls, served by picoserve over our smoltcp
//! sockets.
//!
//! Each server socket has a connection future that listens, serves requests with keep-alive and
//! listens again. [`HttpServer::poll`] polls every one of them once per network poll with a no-op
//! waker, so a connection advances only as far as its socket buffers allow and the poll stays
//! short. The futures reach the sockets through [`Shared`], which holds the socket set for the
//! length of that poll.
//!
//! The status document is serialized by [`Cache`], at most once per poll and only when a
//! connection asks for one that is not current. A connection that finds none parks, the poll
//! renders, and every connection is polled again; requests and event streams then send the same
//! bytes. Nothing on the request path allocates once the server is running, except a render while
//! a slow client still holds the previous document.

#![expect(
    clippy::future_not_send,
    reason = "the server's futures live and are polled on the BSP only, sharing state through Rc and RefCell"
)]

mod api;
mod doc;
mod json;
mod socket;
#[cfg(test)]
mod testing;

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::convert::Infallible;
use core::future::poll_fn;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use core::time::Duration;

use ferrite_hal::Instant;
use picoserve::io::Read;
use picoserve::request::{Path, Request};
use picoserve::response::sse::{EventSource, EventWriter};
use picoserve::response::{Connection, Content, EventStream, IntoResponse as _, Response, ResponseWriter, StatusCode};
use picoserve::routing::PathRouterService;
use picoserve::{Config, ResponseSent, Router, Server, Timeouts, Timer as _};
use smoltcp::iface::{SocketHandle, SocketSet};
use smoltcp::socket::tcp;

use self::api::{Endpoint, Rejection};
use self::doc::{Cache, Document};
use self::socket::{Clock, Conn};
use super::NetStatus;
use crate::control::{Command, Commands};
use crate::report::Snapshot;

const HTTP_PORT: u16 = 80;
/// Server sockets: a browser's six parallel connections and the event stream, with room for a
/// second client, since a SYN that finds no listening socket is refused.
pub const HTTP_SOCKETS: usize = 16;
/// Event streams open at once; more get a 503 and the page polls instead.
const MAX_STREAMS: usize = 3;
const RX_BUFFER: usize = 2048;
const TX_BUFFER: usize = 8192;
/// Holds one request's line, headers and body.
const REQUEST_BUFFER: usize = 2048;
/// The shortest gap between two events on a stream, so a run that changes every few milliseconds
/// still sends one a second.
const EVENT_INTERVAL: Duration = Duration::from_secs(1);
/// A stream with nothing to say sends a comment this often, so idle connections are not dropped
/// by middleboxes and a vanished client is noticed.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
/// How long a quit waits for its reply to be acknowledged.
const QUIT_GRACE: picoserve::time::Duration = picoserve::time::Duration::from_secs(2);
/// After a connection ends, how long its socket may take to close before it is aborted.
const LINGER: Duration = Duration::from_secs(2);
/// A peer that stops acknowledging for this long is dropped.
const TCP_TIMEOUT: smoltcp::time::Duration = smoltcp::time::Duration::from_secs(10);
const TCP_KEEP_ALIVE: smoltcp::time::Duration = smoltcp::time::Duration::from_secs(3);
/// How often [`PollStats`] logs its summary.
const STATS_EVERY: Duration = Duration::from_secs(30);

static CONFIG: Config = Config::new(Timeouts {
    start_read_request: picoserve::time::Duration::from_secs(5),
    persistent_start_read_request: picoserve::time::Duration::from_secs(5),
    read_request: picoserve::time::Duration::from_secs(3),
    write: picoserve::time::Duration::from_secs(5),
})
.keep_connection_alive();

static PAGE: &str = include_str!("page.html");

/// What the connection futures share with the server.
pub struct Shared {
    /// The interface's sockets during [`HttpServer::poll`], an empty set otherwise.
    sockets: RefCell<SocketSet<'static>>,
    now: Cell<Instant>,
    cache: RefCell<Cache>,
    /// Set by a connection that parked for a current document, until the poll renders one.
    wanted: Cell<bool>,
    commands: RefCell<Vec<Command>>,
    streams: Cell<usize>,
    responses: Cell<u32>,
}

impl Shared {
    fn now(&self) -> Instant {
        self.now.get()
    }

    /// The status document, once one that is current exists; the poll renders it on request.
    async fn document(&self) -> Document {
        poll_fn(|_| {
            let document = self.cache.borrow().get(self.now());
            if document.is_none() {
                self.wanted.set(true);
            }
            document.map_or(Poll::Pending, Poll::Ready)
        })
        .await
    }

    /// What the document shows has changed since this generation.
    fn changed_since(&self, generation: Option<u64>) -> bool {
        generation != Some(self.cache.borrow().generation())
    }
}

/// What one call to [`HttpServer::poll`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Served {
    /// Responses sent in full.
    pub responses: u32,
    /// Status documents serialized.
    pub renders: u32,
    /// Some connection has data queued either way, so the next poll should come soon.
    pub busy: bool,
}

/// The cost of [`HttpServer::poll`], summarized to the log every [`STATS_EVERY`].
#[derive(Default)]
pub struct PollStats {
    since: Instant,
    polls: u32,
    total: Duration,
    worst: Duration,
    responses: u32,
    renders: u32,
}

impl PollStats {
    /// Note one poll that began at `now`, took `took` and did `served`.
    pub fn record(&mut self, now: Instant, took: Duration, served: &Served) {
        if self.polls == 0 {
            self.since = now;
        }
        self.polls += 1;
        self.total += took;
        self.worst = self.worst.max(took);
        self.responses += served.responses;
        self.renders += served.renders;
        if now - self.since < STATS_EVERY {
            return;
        }
        log::debug!(
            "http: {} polls in {:?}, mean {:?}, worst {:?}, {} responses, {} renders",
            self.polls,
            now - self.since,
            self.total / self.polls,
            self.worst,
            self.responses,
            self.renders
        );
        *self = Self::default();
    }
}

pub struct HttpServer {
    shared: Rc<Shared>,
    handles: Vec<SocketHandle>,
    connections: Vec<Pin<Box<dyn Future<Output = Infallible>>>>,
}

impl HttpServer {
    pub fn new(sockets: &mut SocketSet<'static>) -> Self {
        let shared = Rc::new(Shared {
            sockets: RefCell::new(SocketSet::new(vec![])),
            now: Cell::new(Instant::default()),
            cache: RefCell::new(Cache::new()),
            wanted: Cell::new(false),
            commands: RefCell::new(Vec::new()),
            streams: Cell::new(0),
            responses: Cell::new(0),
        });
        let handles: Vec<_> = (0..HTTP_SOCKETS)
            .map(|_| {
                let mut sock = tcp::Socket::new(
                    tcp::SocketBuffer::new(vec![0; RX_BUFFER]),
                    tcp::SocketBuffer::new(vec![0; TX_BUFFER]),
                );
                sock.set_timeout(Some(TCP_TIMEOUT));
                sock.set_keep_alive(Some(TCP_KEEP_ALIVE));
                // picoserve writes the head and the body separately; with Nagle the body would wait
                // for the head's ACK, which a peer may delay by hundreds of milliseconds.
                sock.set_nagle_enabled(false);
                sockets.add(sock)
            })
            .collect();
        let connections = handles
            .iter()
            .map(|&handle| -> Pin<Box<dyn Future<Output = Infallible>>> { Box::pin(serve(Rc::clone(&shared), handle)) })
            .collect();
        Self { shared, handles, connections }
    }

    /// Advance every connection by what its socket allows. Control requests are queued on
    /// `commands`; `snapshot` and `net` are what status requests see.
    pub fn poll(
        &mut self,
        sockets: &mut SocketSet<'static>,
        now: Instant,
        snapshot: &Snapshot<'_>,
        net: NetStatus,
        commands: &mut Commands,
    ) -> Served {
        let shared = &*self.shared;
        shared.now.set(now);
        shared.cache.borrow_mut().update(snapshot, net);
        core::mem::swap(&mut *shared.sockets.borrow_mut(), sockets);
        poll_all(&mut self.connections);
        let renders = u32::from(shared.wanted.take());
        if renders > 0 {
            // Connections that parked for a document get it, and the rest of what they were doing,
            // in the same poll.
            shared.cache.borrow_mut().render(snapshot, now);
            poll_all(&mut self.connections);
        }
        core::mem::swap(&mut *shared.sockets.borrow_mut(), sockets);

        for command in shared.commands.borrow_mut().drain(..) {
            commands.push(command);
        }
        let busy = self.handles.iter().any(|&h| {
            let sock = sockets.get::<tcp::Socket<'_>>(h);
            sock.send_queue() > 0 || sock.recv_queue() > 0
        });
        Served { responses: shared.responses.replace(0), renders, busy }
    }
}

fn poll_all(connections: &mut [Pin<Box<dyn Future<Output = Infallible>>>]) {
    let mut cx = Context::from_waker(Waker::noop());
    for connection in connections {
        // The futures loop forever, so this never yields a value.
        let _ = connection.as_mut().poll(&mut cx);
    }
}

/// One server socket's life: accept, serve until the client is done, settle, again.
async fn serve(shared: Rc<Shared>, handle: SocketHandle) -> Infallible {
    let app = Router::from_service(Api { shared: &shared, handle });
    let mut buffer = vec![0; REQUEST_BUFFER];
    loop {
        let conn = Conn::new(&shared, handle);
        conn.accept(HTTP_PORT).await;
        let remote = conn.remote();
        let server = Server::custom(&app, Clock(&shared), &CONFIG, &mut buffer);
        if let Err(e) = server.serve(conn).await {
            log::debug!("http: connection from {remote:?} dropped: {e}");
        }
        conn.settle(LINGER).await;
    }
}

/// Routes every request; picoserve's own router is not used, so routing stays testable on its own.
struct Api<'a> {
    shared: &'a Shared,
    handle: SocketHandle,
}

impl PathRouterService for Api<'_> {
    async fn call_path_router_service<R: Read, W: ResponseWriter<Error = R::Error>>(
        &self,
        _state: &(),
        _path_parameters: (),
        path: Path<'_>,
        request: Request<'_, R>,
        writer: W,
    ) -> Result<ResponseSent, W::Error> {
        let sent = self.respond(path.encoded(), request, writer).await;
        self.shared.responses.set(self.shared.responses.get().wrapping_add(1));
        sent
    }
}

impl Api<'_> {
    async fn respond<R: Read, W: ResponseWriter<Error = R::Error>>(
        &self,
        path: &str,
        mut request: Request<'_, R>,
        writer: W,
    ) -> Result<ResponseSent, W::Error> {
        let host = request.parts.headers().get("host").map(|v| v.as_raw());
        let address = self.shared.cache.borrow().address();
        let endpoint = api::check_host(host, address).and_then(|()| api::resolve(request.parts.method(), path));
        let control = match endpoint {
            Ok(Endpoint::Control(control)) => control,
            Ok(endpoint) => {
                let conn = request.body_connection.finalize().await?;
                return self.get(endpoint, conn, writer).await;
            },
            Err(rejection) => {
                let conn = request.body_connection.finalize().await?;
                return reject(&rejection, conn, writer).await;
            },
        };
        let content_type = request.parts.headers().get("content-type").map(|v| v.as_raw());
        let parsed = match api::check_json(content_type) {
            Err(rejection) => Err(rejection),
            Ok(()) if request.body_connection.content_length() > api::BODY_CAP => Err(Rejection::TooLarge),
            Ok(()) => request.body_connection.body().read_all().await.map_or(Err(Rejection::Unreadable), |body| {
                api::command(control, body).map_err(|source| Rejection::BadBody { source })
            }),
        };
        let conn = request.body_connection.finalize().await?;
        let command = match parsed {
            Ok(command) => command,
            Err(rejection) => return reject(&rejection, conn, writer).await,
        };
        let sent = send(conn, writer, StatusCode::ACCEPTED, json::accepted(control.name()), None).await?;
        if command == Command::Quit {
            // The app ends right after the quit is applied, so let the reply reach the client first.
            let clock = Clock(self.shared);
            let acked = clock.run_with_timeout(QUIT_GRACE, Conn::new(self.shared, self.handle).drained()).await;
            if acked.is_err() {
                log::debug!("http: quit reply not acknowledged within {QUIT_GRACE}, quitting anyway");
            }
        }
        log::debug!("http: {command:?} requested");
        self.shared.commands.borrow_mut().push(command);
        Ok(sent)
    }

    async fn get<R: Read, W: ResponseWriter<Error = R::Error>>(
        &self,
        endpoint: Endpoint,
        conn: Connection<'_, R>,
        writer: W,
    ) -> Result<ResponseSent, W::Error> {
        match endpoint {
            Endpoint::Page => {
                Response::ok(PAGE)
                    .with_content_type("text/html; charset=utf-8")
                    .with_header("Cache-Control", "no-store")
                    .write_to(conn, writer)
                    .await
            },
            Endpoint::Status => send(conn, writer, StatusCode::OK, self.shared.document().await, None).await,
            Endpoint::Events => match StreamSlot::claim(self.shared) {
                Some(slot) => EventStream(Updates(slot)).write_to(conn, writer).await,
                None => reject(&Rejection::Busy, conn, writer).await,
            },
            Endpoint::Control(_) => reject(&Rejection::NotFound, conn, writer).await,
        }
    }
}

async fn send<R: Read, W: ResponseWriter<Error = R::Error>>(
    conn: Connection<'_, R>,
    writer: W,
    status: StatusCode,
    body: impl Content,
    allow: Option<&'static str>,
) -> Result<ResponseSent, W::Error> {
    Response::new(status, body)
        .with_content_type("application/json")
        .with_header("Cache-Control", "no-store")
        .with_headers(allow.map(|methods| ("Allow", methods)))
        .write_to(conn, writer)
        .await
}

async fn reject<R: Read, W: ResponseWriter<Error = R::Error>>(
    rejection: &Rejection,
    conn: Connection<'_, R>,
    writer: W,
) -> Result<ResponseSent, W::Error> {
    let allow = match rejection {
        Rejection::MethodNotAllowed { allow } => Some(*allow),
        _ => None,
    };
    send(conn, writer, StatusCode::new(rejection.status()), json::error(rejection), allow).await
}

/// One of the [`MAX_STREAMS`] event streams, given back on drop.
struct StreamSlot<'a>(&'a Shared);

impl<'a> StreamSlot<'a> {
    fn claim(shared: &'a Shared) -> Option<Self> {
        let open = shared.streams.get();
        (open < MAX_STREAMS).then(|| {
            shared.streams.set(open + 1);
            Self(shared)
        })
    }
}

impl Drop for StreamSlot<'_> {
    fn drop(&mut self) {
        self.0.streams.set(self.0.streams.get() - 1);
    }
}

/// What a stream sends next.
enum Due {
    Event,
    Keepalive,
}

/// The status document as a `status` event whenever what it shows changes (at most one per
/// [`EVENT_INTERVAL`]), and a comment every [`KEEPALIVE_INTERVAL`] that nothing else was sent.
struct Updates<'a>(StreamSlot<'a>);

impl EventSource for Updates<'_> {
    async fn write_events<W: picoserve::io::Write>(self, mut writer: EventWriter<'_, W>) -> Result<(), W::Error> {
        let shared = self.0.0;
        let mut sent = None;
        let mut last_event = shared.now();
        let mut last_write = last_event;
        loop {
            let due = poll_fn(|_| {
                let now = shared.now();
                if shared.changed_since(sent) && (sent.is_none() || now - last_event >= EVENT_INTERVAL) {
                    Poll::Ready(Due::Event)
                } else if now - last_write >= KEEPALIVE_INTERVAL {
                    Poll::Ready(Due::Keepalive)
                } else {
                    Poll::Pending
                }
            })
            .await;
            match due {
                Due::Event => {
                    let document = shared.document().await;
                    writer.write_event("status", document.as_str()).await?;
                    sent = Some(document.generation());
                    last_event = shared.now();
                    last_write = last_event;
                },
                Due::Keepalive => {
                    writer.write_keepalive().await?;
                    last_write = shared.now();
                },
            }
            writer.flush().await?;
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::collections::VecDeque;
    use alloc::format;
    use alloc::string::String;
    use core::num::NonZeroUsize;

    use assert2::check;
    use rstest::rstest;
    use smoltcp::wire::{Ipv4Address, Ipv4Cidr};

    use super::testing::{count, idle_status};
    use super::*;
    use crate::report::{FaultRecord, History, PassRecord};
    use crate::sweep::{CoreChoice, SweepStatus};

    const HOST: &str = "Host: ferrite.local\r\n";

    /// The server and one client socket on a smoltcp loopback interface, stepped a millisecond at a time.
    struct Loopback {
        device: smoltcp::phy::Loopback,
        iface: smoltcp::iface::Interface,
        sockets: SocketSet<'static>,
        http: HttpServer,
        client: SocketHandle,
        ms: u64,
        served: u32,
        renders: u32,
        allocs: u64,
        commands: Commands,
        status: SweepStatus,
        net: NetStatus,
        history: History,
        faults: VecDeque<FaultRecord>,
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
                .add(tcp::Socket::new(tcp::SocketBuffer::new(vec![0; 65536]), tcp::SocketBuffer::new(vec![0; 4096])));
            Self {
                device,
                iface,
                sockets,
                http,
                client,
                ms: 0,
                served: 0,
                renders: 0,
                allocs: 0,
                commands: Commands::default(),
                status: idle_status(),
                net: NetStatus::default(),
                history: History::default(),
                faults: VecDeque::new(),
                reply: Vec::new(),
            }
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

        /// Step until `done` holds for the reply so far, at most `limit` milliseconds.
        fn run_until(&mut self, mut done: impl FnMut(&tcp::Socket<'static>) -> bool, limit: u64) {
            for _ in 0..limit {
                self.ms += 1;
                let at = smoltcp::time::Instant::from_millis(self.ms.cast_signed());
                self.iface.poll(at, &mut self.device, &mut self.sockets);
                let now = Instant::from_epoch(Duration::from_millis(self.ms));
                let snapshot = Snapshot { status: self.status, history: &self.history, faults: &self.faults };
                let (served, allocs) =
                    count(|| self.http.poll(&mut self.sockets, now, &snapshot, self.net, &mut self.commands));
                self.served += served.responses;
                self.renders += served.renders;
                self.allocs += allocs;
                self.iface.poll_egress(at, &mut self.device, &mut self.sockets);
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

        fn request(&mut self, request: &str) {
            check!(self.client().send_slice(request.as_bytes()).unwrap() == request.len());
        }

        fn reply(&self) -> String {
            String::from_utf8_lossy(&self.reply).into_owned()
        }

        fn commands(&mut self) -> Vec<Command> {
            self.commands.drain().collect()
        }

        /// The number of status events and keepalive comments in the reply so far.
        fn events(&self) -> (usize, usize) {
            let reply = self.reply();
            (reply.matches("event:status\ndata:{\"uptime_ms\":").count(), reply.matches(":\n\n").count())
        }
    }

    #[rstest]
    #[case::page("GET / HTTP/1.1\r\nHost: ferrite.local\r\n\r\n", "HTTP/1.1 200 \r\n", "<!doctype html>")]
    #[case::status(
        "GET /api/status HTTP/1.1\r\nHost: ferrite.local\r\n\r\n",
        "HTTP/1.1 200 \r\n",
        "\"preset\":\"standard\""
    )]
    #[case::not_found(
        "GET /favicon.ico HTTP/1.1\r\nHost: ferrite.local\r\n\r\n",
        "HTTP/1.1 404 \r\n",
        "no such resource"
    )]
    #[case::wrong_method(
        "GET /api/quit HTTP/1.1\r\nHost: ferrite.local\r\n\r\n",
        "HTTP/1.1 405 \r\n",
        "Allow: POST\r\n"
    )]
    #[case::form_post(
        "POST /api/quit HTTP/1.1\r\nHost: ferrite.local\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: 0\r\n\r\n",
        "HTTP/1.1 415 \r\n",
        "application/json"
    )]
    #[case::bad_body(
        "POST /api/restart HTTP/1.1\r\nHost: ferrite.local\r\nContent-Type: application/json\r\nContent-Length: 17\r\n\r\n{\"preset\":\"fast\"}",
        "HTTP/1.1 400 \r\n",
        "unknown preset"
    )]
    #[case::no_host("GET /api/status HTTP/1.1\r\n\r\n", "HTTP/1.1 400 \r\n", "missing Host header")]
    #[case::foreign_status(
        "GET /api/status HTTP/1.1\r\nHost: evil.example\r\n\r\n",
        "HTTP/1.1 421 \r\n",
        "use ferrite.local"
    )]
    #[case::foreign_page("GET / HTTP/1.1\r\nHost: evil.example:80\r\n\r\n", "HTTP/1.1 421 \r\n", "use ferrite.local")]
    #[case::foreign_events(
        "GET /api/events HTTP/1.1\r\nHost: evil.example\r\n\r\n",
        "HTTP/1.1 421 \r\n",
        "use ferrite.local"
    )]
    #[case::foreign_control(
        "POST /api/quit HTTP/1.1\r\nHost: evil.example\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
        "HTTP/1.1 421 \r\n",
        "use ferrite.local"
    )]
    fn answers(#[case] request: &str, #[case] status_line: &str, #[case] contains: &str) {
        let mut net = Loopback::new();
        net.connect(49_152);
        net.request(request);
        net.run(200);
        let reply = net.reply();
        check!(reply.starts_with(status_line), "{reply}");
        check!(reply.contains(contains), "{reply}");
        check!(net.served == 1);
        check!(net.commands().is_empty());
    }

    #[rstest]
    #[case::our_address("10.0.2.15", "HTTP/1.1 200 \r\n")]
    #[case::our_address_and_port("10.0.2.15:80", "HTTP/1.1 200 \r\n")]
    #[case::next_address("10.0.2.16", "HTTP/1.1 421 \r\n")]
    fn the_leased_address_is_the_one_accepted(#[case] host: &str, #[case] status_line: &str) {
        let mut net = Loopback::new();
        net.net.link.address = Some(Ipv4Cidr::new(Ipv4Address::new(10, 0, 2, 15), 24));
        net.connect(49_152);
        net.request(&format!("GET /api/status HTTP/1.1\r\nHost: {host}\r\n\r\n"));
        net.run(200);
        check!(net.reply().starts_with(status_line), "{}", net.reply());
    }

    #[test]
    fn a_control_post_queues_its_command() {
        let mut net = Loopback::new();
        net.connect(49_152);
        net.request(&format!(
            "POST /api/cores HTTP/1.1\r\n{HOST}Content-Type: application/json\r\nContent-Length: 11\r\n\r\n{{\"cores\":2}}"
        ));
        net.run(200);
        check!(net.reply().starts_with("HTTP/1.1 202 \r\n"));
        check!(net.reply().ends_with("{\"accepted\":\"cores\"}"));
        check!(net.commands() == [Command::Cores(CoreChoice::Count(NonZeroUsize::new(2).unwrap()))]);
    }

    #[test]
    fn keep_alive_serves_two_requests_on_one_connection() {
        let mut net = Loopback::new();
        net.connect(49_152);
        net.request(&format!("GET /api/status HTTP/1.1\r\n{HOST}\r\n"));
        net.run(100);
        net.request(&format!(
            "POST /api/pause HTTP/1.1\r\n{HOST}Content-Type: application/json\r\nContent-Length: 0\r\n\r\n"
        ));
        net.run(100);
        let reply = net.reply();
        check!(reply.starts_with("HTTP/1.1 200 \r\n"));
        check!(reply.matches("HTTP/1.1 ").count() == 2);
        check!(reply.contains("HTTP/1.1 202 \r\n"));
        check!(net.client().may_send(), "still open");
        check!(net.served == 2);
        check!(net.commands() == [Command::Pause]);
    }

    #[test]
    fn an_idle_kept_alive_connection_is_closed_and_the_socket_listens_again() {
        let mut net = Loopback::new();
        net.connect(49_152);
        net.request(&format!("GET /api/status HTTP/1.1\r\n{HOST}\r\n"));
        net.run_until(|sock| !sock.may_recv(), 10_000);
        check!(!net.client().may_recv(), "the server closed its side");
        net.client().close();
        net.run(100);

        net.connect(49_153);
        net.request(&format!("GET / HTTP/1.1\r\n{HOST}\r\n"));
        net.run(200);
        check!(net.reply().starts_with("HTTP/1.1 200 \r\n"));
        check!(net.served == 2);
    }

    #[test]
    fn requests_close_together_share_one_render() {
        let mut net = Loopback::new();
        net.connect(49_152);
        for _ in 0..5 {
            net.request(&format!("GET /api/status HTTP/1.1\r\n{HOST}\r\n"));
            net.run(20);
        }
        check!(net.served == 5);
        check!(net.renders == 1);

        net.history.revision += 1;
        net.request(&format!("GET /api/status HTTP/1.1\r\n{HOST}\r\n"));
        net.run(20);
        check!(net.served == 6);
        check!(net.renders == 2);
    }

    #[test]
    fn the_event_stream_sends_the_first_status_at_once_then_only_changes() {
        let mut net = Loopback::new();
        net.connect(49_152);
        net.request(&format!("GET /api/events HTTP/1.1\r\n{HOST}\r\n"));
        net.run(2_500);
        let reply = net.reply();
        check!(reply.starts_with("HTTP/1.1 200 \r\n"));
        check!(reply.contains("Content-Type: text/event-stream\r\n"));
        check!(net.events() == (1, 0), "{reply}");
        check!(net.http.shared.streams.get() == 1);

        net.history.revision += 1;
        net.run(1_100);
        check!(net.events() == (2, 0));

        net.net.counters.rx_frames += 40;
        net.run(3_000);
        check!(net.events() == (2, 0), "counters alone are not a change");

        net.client().abort();
        net.run(100);
        check!(net.http.shared.streams.get() == 0, "the stream's slot was given back");
    }

    #[test]
    fn a_busy_run_still_sends_one_event_a_second_at_most() {
        let mut net = Loopback::new();
        net.connect(49_152);
        net.request(&format!("GET /api/events HTTP/1.1\r\n{HOST}\r\n"));
        net.run(100);
        for _ in 0..30 {
            net.history.revision += 1;
            net.run(100);
        }
        let (events, _) = net.events();
        check!((3..=4).contains(&events), "{events} events in about three seconds");
    }

    #[test]
    fn an_idle_stream_sends_a_comment_every_fifteen_seconds() {
        let mut net = Loopback::new();
        net.connect(49_152);
        net.request(&format!("GET /api/events HTTP/1.1\r\n{HOST}\r\n"));
        net.run(14_500);
        check!(net.events() == (1, 0));
        net.run(1_000);
        check!(net.events() == (1, 1));
        net.run(14_000);
        check!(net.events() == (1, 1));
        net.run(1_000);
        check!(net.events() == (1, 2));
    }

    #[test]
    fn a_running_server_makes_no_allocations() {
        let mut net = Loopback::new();
        for pass in 0..64 {
            net.history.passes.push_back(PassRecord { pass, took: Duration::from_secs(90), faults: 3 });
            net.faults.push_back(FaultRecord {
                fault: ferrite_mem::Fault {
                    addr: ferrite_mem::CpuAddr::of(core::ptr::without_provenance::<u8>(0x4000_1008)),
                    expected: !0,
                    actual: !0 ^ 0x10,
                },
                test: "moving-inversions",
                cpu: 3,
                pass,
            });
        }
        net.connect(49_152);
        let cases = [
            format!("GET /api/status HTTP/1.1\r\n{HOST}\r\n"),
            format!("GET / HTTP/1.1\r\n{HOST}\r\n"),
            format!("GET /nope HTTP/1.1\r\n{HOST}\r\n"),
            "GET /api/status HTTP/1.1\r\nHost: evil.example\r\n\r\n".into(),
            format!("POST /api/pause HTTP/1.1\r\n{HOST}Content-Type: application/json\r\nContent-Length: 0\r\n\r\n"),
            format!(
                "POST /api/restart HTTP/1.1\r\n{HOST}Content-Type: application/json\r\nContent-Length: 17\r\n\r\n{{\"preset\":\"fast\"}}"
            ),
        ];
        for request in &cases {
            net.request(request);
            net.run(30);
            net.commands();
        }
        let (served, allocs) = (net.served, net.allocs);
        for request in cases.iter().cycle().take(60) {
            net.history.revision += 1;
            net.request(request);
            net.run(30);
            net.commands();
        }
        check!(net.served - served == 60);
        check!(net.allocs == allocs, "{} allocations in 60 requests", net.allocs - allocs);

        net.client().abort();
        net.run(2_000);
        net.connect(49_153);
        net.request(&format!("GET /api/events HTTP/1.1\r\n{HOST}\r\n"));
        net.run(1_500);
        let allocs = net.allocs;
        for _ in 0..5 {
            net.history.revision += 1;
            net.run(1_100);
        }
        check!(net.events().0 >= 5);
        check!(net.allocs == allocs, "{} allocations in 5 events", net.allocs - allocs);
    }
}
