//! The status page, its JSON API and the run's controls, served by picoserve over our smoltcp
//! sockets.
//!
//! Each server socket has a connection future that listens, serves requests with keep-alive and
//! listens again. [`HttpServer::poll`] polls every one of them once per network poll with a no-op
//! waker, so a connection advances only as far as its socket buffers allow and the poll stays
//! short. The futures reach the sockets through [`Shared`], which holds the socket set for the
//! length of that poll, and read the run from a copy of the sweep's snapshot kept there.

#![expect(
    clippy::future_not_send,
    reason = "the server's futures live and are polled on the BSP only, sharing state through Rc and RefCell"
)]

mod api;
mod json;
mod socket;

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::convert::Infallible;
use core::pin::Pin;
use core::task::{Context, Waker};
use core::time::Duration;

use ferrite_hal::Instant;
use picoserve::io::Read;
use picoserve::request::{Path, Request};
use picoserve::response::sse::{EventSource, EventWriter};
use picoserve::response::{Connection, EventStream, IntoResponse as _, Response, ResponseWriter, StatusCode};
use picoserve::routing::PathRouterService;
use picoserve::{Config, ResponseSent, Router, Server, Timeouts, Timer as _};
use smoltcp::iface::{SocketHandle, SocketSet};
use smoltcp::socket::tcp;

use self::api::{Endpoint, Rejection};
use self::json::{StatusDoc, ToJson as _};
use self::socket::{Clock, Conn};
use super::NetStatus;
use crate::control::{Command, Commands};
use crate::report::{FaultRecord, History, Snapshot};
use crate::sweep::SweepStatus;

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
const EVENT_INTERVAL: Duration = Duration::from_secs(1);
/// How long a quit waits for its reply to be acknowledged.
const QUIT_GRACE: picoserve::time::Duration = picoserve::time::Duration::from_secs(2);
/// After a connection ends, how long its socket may take to close before it is aborted.
const LINGER: Duration = Duration::from_secs(2);
/// A peer that stops acknowledging for this long is dropped.
const TCP_TIMEOUT: smoltcp::time::Duration = smoltcp::time::Duration::from_secs(10);
const TCP_KEEP_ALIVE: smoltcp::time::Duration = smoltcp::time::Duration::from_secs(3);

static CONFIG: Config = Config::new(Timeouts {
    start_read_request: picoserve::time::Duration::from_secs(5),
    persistent_start_read_request: picoserve::time::Duration::from_secs(5),
    read_request: picoserve::time::Duration::from_secs(3),
    write: picoserve::time::Duration::from_secs(5),
})
.keep_connection_alive();

static PAGE: &str = include_str!("page.html");

/// The sweep as last seen by the network poll, owned so requests in flight can read it.
struct Mirror {
    status: SweepStatus,
    history: History,
    faults: VecDeque<FaultRecord>,
    net: NetStatus,
}

impl Mirror {
    fn new(snapshot: &Snapshot<'_>, net: NetStatus) -> Self {
        Self { status: snapshot.status, history: snapshot.history.clone(), faults: snapshot.faults.clone(), net }
    }

    /// Copy what changed; the history and fault log only when their revision or count moved.
    fn update(&mut self, snapshot: &Snapshot<'_>, net: NetStatus) {
        if self.history.revision != snapshot.history.revision {
            self.history.clone_from(snapshot.history);
        }
        if self.status.faults != snapshot.status.faults || self.faults.len() != snapshot.faults.len() {
            self.faults.clone_from(snapshot.faults);
        }
        self.status = snapshot.status;
        self.net = net;
    }
}

/// What the connection futures share with the server.
pub struct Shared {
    /// The interface's sockets during [`HttpServer::poll`], an empty set otherwise.
    sockets: RefCell<SocketSet<'static>>,
    now: Cell<Instant>,
    mirror: RefCell<Option<Mirror>>,
    commands: RefCell<Vec<Command>>,
    streams: Cell<usize>,
    responses: Cell<u32>,
}

impl Shared {
    fn now(&self) -> Instant {
        self.now.get()
    }

    fn status_json(&self) -> Option<String> {
        let mirror = self.mirror.borrow();
        let mirror = mirror.as_ref()?;
        let snapshot = Snapshot { status: mirror.status, history: &mirror.history, faults: &mirror.faults };
        let mut out = String::with_capacity(4096);
        StatusDoc { snapshot: &snapshot, net: &mirror.net, now: self.now() }.to_json(&mut out);
        Some(out)
    }
}

/// What one call to [`HttpServer::poll`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Served {
    /// Responses sent in full.
    pub responses: u32,
    /// Some connection has data queued either way, so the next poll should come soon.
    pub busy: bool,
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
            mirror: RefCell::new(None),
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
        match &mut *shared.mirror.borrow_mut() {
            Some(mirror) => mirror.update(snapshot, net),
            empty @ None => *empty = Some(Mirror::new(snapshot, net)),
        }
        core::mem::swap(&mut *shared.sockets.borrow_mut(), sockets);
        let mut cx = Context::from_waker(Waker::noop());
        for connection in &mut self.connections {
            // The futures loop forever, so this never yields a value.
            let _ = connection.as_mut().poll(&mut cx);
        }
        core::mem::swap(&mut *shared.sockets.borrow_mut(), sockets);

        for command in shared.commands.borrow_mut().drain(..) {
            commands.push(command);
        }
        let busy = self.handles.iter().any(|&h| {
            let sock = sockets.get::<tcp::Socket<'_>>(h);
            sock.send_queue() > 0 || sock.recv_queue() > 0
        });
        Served { responses: shared.responses.replace(0), busy }
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
        let endpoint = api::resolve(request.parts.method(), path);
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
            Endpoint::Status => match self.shared.status_json() {
                Some(doc) => send(conn, writer, StatusCode::OK, doc, None).await,
                None => reject(&Rejection::NotFound, conn, writer).await,
            },
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
    body: String,
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

/// The status document as a `status` event every [`EVENT_INTERVAL`].
struct Updates<'a>(StreamSlot<'a>);

impl EventSource for Updates<'_> {
    async fn write_events<W: picoserve::io::Write>(self, mut writer: EventWriter<'_, W>) -> Result<(), W::Error> {
        let shared = self.0.0;
        loop {
            match shared.status_json() {
                Some(doc) => writer.write_event("status", doc.as_str()).await?,
                None => writer.write_keepalive().await?,
            }
            writer.flush().await?;
            Clock(shared).sleep(EVENT_INTERVAL).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use core::num::NonZeroUsize;

    use assert2::check;
    use ferrite_mem::analysis::Classification;
    use ferrite_mem::suite::{Mode, Preset};
    use rstest::rstest;

    use super::*;
    use crate::sweep::CoreChoice;

    fn idle_status() -> SweepStatus {
        SweepStatus {
            bytes: 1 << 20,
            test: "own-address",
            position: (1, 9),
            test_percent: 0,
            percent: 0,
            passes: 0,
            tier: "sse2",
            mode: Mode::Single,
            cpus: 1,
            faults: 0,
            first_fault: None,
            first_fault_test: "",
            first_fault_cpu: 0,
            analysis: Classification::Clean,
            preset: Preset::Standard,
            paused: false,
        }
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
        commands: Commands,
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
                commands: Commands::default(),
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
                let snapshot = Snapshot { status: idle_status(), history: &self.history, faults: &self.faults };
                let served =
                    self.http.poll(&mut self.sockets, now, &snapshot, NetStatus::default(), &mut self.commands);
                self.served += served.responses;
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
    }

    #[rstest]
    #[case::page("GET / HTTP/1.1\r\n\r\n", "HTTP/1.1 200 \r\n", "<!doctype html>")]
    #[case::status("GET /api/status HTTP/1.1\r\n\r\n", "HTTP/1.1 200 \r\n", "\"preset\":\"standard\"")]
    #[case::not_found("GET /favicon.ico HTTP/1.1\r\n\r\n", "HTTP/1.1 404 \r\n", "no such resource")]
    #[case::wrong_method("GET /api/quit HTTP/1.1\r\n\r\n", "HTTP/1.1 405 \r\n", "Allow: POST\r\n")]
    #[case::form_post(
        "POST /api/quit HTTP/1.1\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: 0\r\n\r\n",
        "HTTP/1.1 415 \r\n",
        "application/json"
    )]
    #[case::bad_body(
        "POST /api/restart HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 17\r\n\r\n{\"preset\":\"fast\"}",
        "HTTP/1.1 400 \r\n",
        "unknown preset"
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

    #[test]
    fn a_control_post_queues_its_command() {
        let mut net = Loopback::new();
        net.connect(49_152);
        net.request(
            "POST /api/cores HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 11\r\n\r\n{\"cores\":2}",
        );
        net.run(200);
        check!(net.reply().starts_with("HTTP/1.1 202 \r\n"));
        check!(net.reply().ends_with("{\"accepted\":\"cores\"}"));
        check!(net.commands() == [Command::Cores(CoreChoice::Count(NonZeroUsize::new(2).unwrap()))]);
    }

    #[test]
    fn keep_alive_serves_two_requests_on_one_connection() {
        let mut net = Loopback::new();
        net.connect(49_152);
        net.request("GET /api/status HTTP/1.1\r\n\r\n");
        net.run(100);
        net.request("POST /api/pause HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 0\r\n\r\n");
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
        net.request("GET /api/status HTTP/1.1\r\n\r\n");
        net.run_until(|sock| !sock.may_recv(), 10_000);
        check!(!net.client().may_recv(), "the server closed its side");
        net.client().close();
        net.run(100);

        net.connect(49_153);
        net.request("GET / HTTP/1.1\r\n\r\n");
        net.run(200);
        check!(net.reply().starts_with("HTTP/1.1 200 \r\n"));
        check!(net.served == 2);
    }

    #[test]
    fn the_event_stream_sends_the_status_every_second() {
        let mut net = Loopback::new();
        net.connect(49_152);
        net.request("GET /api/events HTTP/1.1\r\n\r\n");
        net.run(2_500);
        let reply = net.reply();
        check!(reply.starts_with("HTTP/1.1 200 \r\n"));
        check!(reply.contains("Content-Type: text/event-stream\r\n"));
        check!(reply.matches("event:status\ndata:{\"uptime_ms\":").count() == 3, "{reply}");
        check!(net.http.shared.streams.get() == 1);

        net.client().abort();
        net.run(100);
        check!(net.http.shared.streams.get() == 0, "the stream's slot was given back");
    }
}
