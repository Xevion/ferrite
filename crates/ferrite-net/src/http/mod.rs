//! The HTTP server every stack runs, generic over the app's [`Site`], served by picoserve over our
//! smoltcp sockets.
//!
//! Each server socket has a connection future that listens, serves requests with keep-alive and
//! listens again. [`Server::poll`] polls every one of them once per network poll with a no-op
//! waker, so a connection advances only as far as its socket buffers allow and the poll stays
//! short. The futures reach the sockets through [`Shared`], which holds the socket set for the
//! length of that poll.
//!
//! The site's document is serialized by [`Doc`], at most once per poll and only when a connection
//! asks for one that is not current, and shared by the servers of every stack. A connection that
//! finds none parks, the poll renders, and every connection is polled again; requests and event
//! streams then send the same bytes. Nothing on the request path allocates once the server is
//! running, except a render while a slow client still holds the previous document.

#![expect(
    clippy::future_not_send,
    reason = "the server's futures live and are polled on one core only, sharing state through Rc and RefCell"
)]

mod api;
mod doc;
mod socket;

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::convert::Infallible;
use core::fmt;
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
use picoserve::{Config, ResponseSent, Router, Server as Picoserve, Timeouts, Timer as _};
use smoltcp::iface::{SocketHandle, SocketSet};
use smoltcp::socket::tcp;
use smoltcp::wire::Ipv4Address;

pub use self::api::{BODY_CAP, JsonEscape, Reason, Rejection, Reply, Route, check_host, check_json};
pub use self::doc::{Doc, Document, MAX_AGE};
use self::socket::{Clock, Conn};
use crate::status::NetStatus;

const HTTP_PORT: u16 = 80;
/// Server sockets per stack: a browser's six parallel connections and the event stream, with room
/// for a second client, since a SYN that finds no listening socket is refused.
pub const HTTP_SOCKETS: usize = 16;
/// Event streams open at once on one stack; more get a 503 and the page polls instead.
const MAX_STREAMS: usize = 3;
/// Addresses of every stack the `Host` check accepts.
pub const MAX_ADDRESSES: usize = 8;
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

/// What an app serves: its routes, its control requests and the document it shows.
///
/// One instance serves every stack of a [`crate::Network`]. The server, the `Host` check, the
/// connection handling, event streams and the document cache are the foundation's.
pub trait Site: 'static {
    /// What the app hands each poll: its current state, borrowed for the length of the poll.
    type State<'a>;
    /// A control endpoint, named in the reply to its request.
    type Control: Copy + Into<&'static str>;
    /// The change a control request asks for, queued for the app.
    type Command;
    /// Why a control body does not parse.
    type BodyError: fmt::Display;
    /// What the document shows apart from the clock and the traffic counters; the document is
    /// rendered afresh whenever it changes. The counters change with every frame, including the
    /// acknowledgements of the documents sent, so they never trigger a render by themselves.
    type Key: PartialEq;

    /// The `Content-Type` of the document.
    fn document_type(&self) -> &'static str {
        "application/json"
    }

    /// The name of the event the document is sent as on a stream.
    fn event_name(&self) -> &'static str {
        "status"
    }

    /// The page served for [`Route::Page`].
    fn page(&self) -> &'static str;

    /// The route for `method` on `path` (without the query).
    fn route(&self, method: &str, path: &str) -> Result<Route<Self::Control>, Rejection>;

    /// The command a control request's body asks for.
    fn command(&self, control: Self::Control, body: &[u8]) -> Result<Self::Command, Self::BodyError>;

    /// Whether the app ends once `command` is applied, so its reply is let out first.
    fn ends_run(_command: &Self::Command) -> bool {
        false
    }

    /// What the document shows apart from the clock and the counters.
    fn key(&self, state: &Self::State<'_>) -> Self::Key;

    /// Write the document as of `now` into `out`, which is empty.
    fn render(&self, state: &Self::State<'_>, net: &NetStatus, now: Instant, out: &mut String);
}

/// What the servers of every stack share besides the site: the document and who the machine is.
pub struct Hub {
    doc: RefCell<Doc>,
    /// Every stack's address, for the `Host` check.
    addresses: RefCell<heapless::Vec<Ipv4Address, MAX_ADDRESSES>>,
    /// Set by a connection that parked for a current document, until the poll renders one.
    wanted: Cell<bool>,
}

impl Hub {
    fn new(document_type: &'static str) -> Self {
        Self {
            doc: RefCell::new(Doc::new(document_type)),
            addresses: RefCell::new(heapless::Vec::new()),
            wanted: Cell::new(false),
        }
    }

    /// Replace the addresses the `Host` check accepts.
    pub fn set_addresses(&self, addresses: impl Iterator<Item = Ipv4Address>) {
        let mut slot = self.addresses.borrow_mut();
        slot.clear();
        addresses.take(MAX_ADDRESSES).for_each(|address| {
            let _ = slot.push(address);
        });
    }

    /// What the document shows has changed.
    pub fn changed(&self) {
        self.doc.borrow_mut().bump();
    }
}

/// A site, the commands its control requests queued, and the hub its servers share.
pub struct Web<S: Site> {
    site: S,
    hub: Rc<Hub>,
    commands: RefCell<Vec<S::Command>>,
}

impl<S: Site> Web<S> {
    pub fn new(site: S) -> Rc<Self> {
        let hub = Rc::new(Hub::new(site.document_type()));
        Rc::new(Self { site, hub, commands: RefCell::new(Vec::new()) })
    }

    pub const fn site(&self) -> &S {
        &self.site
    }

    pub fn hub(&self) -> &Hub {
        &self.hub
    }

    /// Hand every queued command to `take`, oldest first.
    pub fn drain_commands(&self, mut take: impl FnMut(S::Command)) {
        self.commands.borrow_mut().drain(..).for_each(&mut take);
    }
}

/// What the connection futures of one server share.
pub struct Shared {
    /// The interface's sockets during [`Server::poll`], an empty set otherwise.
    sockets: RefCell<SocketSet<'static>>,
    now: Cell<Instant>,
    hub: Rc<Hub>,
    streams: Cell<usize>,
    responses: Cell<u32>,
}

impl Shared {
    fn now(&self) -> Instant {
        self.now.get()
    }

    /// The document, once one that is current exists; the poll renders it on request.
    async fn document(&self) -> Document {
        poll_fn(|_| {
            let document = self.hub.doc.borrow().get(self.now());
            if document.is_none() {
                self.hub.wanted.set(true);
            }
            document.map_or(Poll::Pending, Poll::Ready)
        })
        .await
    }

    /// What the document shows has changed since this generation.
    fn changed_since(&self, generation: Option<u64>) -> bool {
        generation != Some(self.hub.doc.borrow().generation())
    }
}

/// What one call to [`Server::poll`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Served {
    /// Responses sent in full.
    pub responses: u32,
    /// Documents serialized.
    pub renders: u32,
    /// Some connection has data queued either way, so the next poll should come soon.
    pub busy: bool,
}

/// The cost of [`Server::poll`], summarized to the log every [`STATS_EVERY`].
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

/// The server of one stack: its sockets and the connection futures that serve them.
pub struct Server {
    shared: Rc<Shared>,
    handles: Vec<SocketHandle>,
    connections: Vec<Pin<Box<dyn Future<Output = Infallible>>>>,
}

impl Server {
    pub fn new<S: Site>(web: &Rc<Web<S>>, sockets: &mut SocketSet<'static>) -> Self {
        let shared = Rc::new(Shared {
            sockets: RefCell::new(SocketSet::new(vec![])),
            now: Cell::new(Instant::default()),
            hub: Rc::clone(&web.hub),
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
            .map(|&handle| -> Pin<Box<dyn Future<Output = Infallible>>> {
                Box::pin(serve(Rc::clone(&shared), Rc::clone(web), handle))
            })
            .collect();
        Self { shared, handles, connections }
    }

    /// Advance every connection by what its socket allows. `render` writes the document when a
    /// connection needs a current one.
    pub fn poll(
        &mut self,
        sockets: &mut SocketSet<'static>,
        now: Instant,
        render: &mut dyn FnMut(&mut String),
    ) -> Served {
        let shared = &*self.shared;
        shared.now.set(now);
        core::mem::swap(&mut *shared.sockets.borrow_mut(), sockets);
        poll_all(&mut self.connections);
        let renders = u32::from(shared.hub.wanted.take());
        if renders > 0 {
            // Connections that parked for a document get it, and the rest of what they were doing,
            // in the same poll.
            shared.hub.doc.borrow_mut().render(now, render);
            poll_all(&mut self.connections);
        }
        core::mem::swap(&mut *shared.sockets.borrow_mut(), sockets);

        let busy = self.handles.iter().any(|&h| {
            let sock = sockets.get::<tcp::Socket<'_>>(h);
            sock.send_queue() > 0 || sock.recv_queue() > 0
        });
        Served { responses: shared.responses.replace(0), renders, busy }
    }

    /// Event streams open on this server.
    pub fn streams(&self) -> usize {
        self.shared.streams.get()
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
async fn serve<S: Site>(shared: Rc<Shared>, web: Rc<Web<S>>, handle: SocketHandle) -> Infallible {
    let app = Router::from_service(Api { shared: &shared, web: &web, handle });
    let mut buffer = vec![0; REQUEST_BUFFER];
    loop {
        let conn = Conn::new(&shared, handle);
        conn.accept(HTTP_PORT).await;
        let remote = conn.remote();
        let server = Picoserve::custom(&app, Clock(&shared), &CONFIG, &mut buffer);
        if let Err(e) = server.serve(conn).await {
            log::debug!("http: connection from {remote:?} dropped: {e}");
        }
        conn.settle(LINGER).await;
    }
}

/// Routes every request; picoserve's own router is not used, so routing is the site's.
struct Api<'a, S: Site> {
    shared: &'a Shared,
    web: &'a Web<S>,
    handle: SocketHandle,
}

impl<S: Site> PathRouterService for Api<'_, S> {
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

impl<S: Site> Api<'_, S> {
    async fn respond<R: Read, W: ResponseWriter<Error = R::Error>>(
        &self,
        path: &str,
        mut request: Request<'_, R>,
        writer: W,
    ) -> Result<ResponseSent, W::Error> {
        let host = request.parts.headers().get("host").map(|v| v.as_raw());
        let route = {
            let addresses = self.shared.hub.addresses.borrow();
            check_host(host, &addresses).and_then(|()| self.web.site.route(request.parts.method(), path))
        };
        let control = match route {
            Ok(Route::Control(control)) => control,
            Ok(route) => {
                let conn = request.body_connection.finalize().await?;
                return self.get(route, conn, writer).await;
            },
            Err(rejection) => {
                let conn = request.body_connection.finalize().await?;
                return reject(&rejection, conn, writer).await;
            },
        };
        let content_type = request.parts.headers().get("content-type").map(|v| v.as_raw());
        let parsed = match check_json(content_type) {
            Err(rejection) => Err(rejection),
            Ok(()) if request.body_connection.content_length() > BODY_CAP => Err(Rejection::TooLarge),
            Ok(()) => request.body_connection.body().read_all().await.map_or(Err(Rejection::Unreadable), |body| {
                self.web.site.command(control, body).map_err(|error| Rejection::bad_body(&error))
            }),
        };
        let conn = request.body_connection.finalize().await?;
        let command = match parsed {
            Ok(command) => command,
            Err(rejection) => return reject(&rejection, conn, writer).await,
        };
        let name: &'static str = control.into();
        let sent = send(conn, writer, StatusCode::ACCEPTED, api::accepted(name), JSON, None).await?;
        if S::ends_run(&command) {
            // The app ends right after the command is applied, so let the reply reach the client first.
            let clock = Clock(self.shared);
            let acked = clock.run_with_timeout(QUIT_GRACE, Conn::new(self.shared, self.handle).drained()).await;
            if acked.is_err() {
                log::debug!("http: {name} reply not acknowledged within {QUIT_GRACE}, applying it anyway");
            }
        }
        log::debug!("http: {name} requested");
        self.web.commands.borrow_mut().push(command);
        Ok(sent)
    }

    async fn get<R: Read, W: ResponseWriter<Error = R::Error>>(
        &self,
        route: Route<S::Control>,
        conn: Connection<'_, R>,
        writer: W,
    ) -> Result<ResponseSent, W::Error> {
        match route {
            Route::Page => {
                Response::ok(self.web.site.page())
                    .with_content_type("text/html; charset=utf-8")
                    .with_header("Cache-Control", "no-store")
                    .write_to(conn, writer)
                    .await
            },
            Route::Document => {
                let document = self.shared.document().await;
                let content_type = document.content_type();
                send(conn, writer, StatusCode::OK, document, content_type, None).await
            },
            Route::Events => match StreamSlot::claim(self.shared) {
                Some(slot) => {
                    EventStream(Updates { slot, event: self.web.site.event_name() }).write_to(conn, writer).await
                },
                None => reject(&Rejection::Busy, conn, writer).await,
            },
            Route::Control(_) => reject(&Rejection::NotFound, conn, writer).await,
        }
    }
}

const JSON: &str = "application/json";

async fn send<R: Read, W: ResponseWriter<Error = R::Error>>(
    conn: Connection<'_, R>,
    writer: W,
    status: StatusCode,
    body: impl Content,
    content_type: &'static str,
    allow: Option<&'static str>,
) -> Result<ResponseSent, W::Error> {
    Response::new(status, body)
        .with_content_type(content_type)
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
    send(conn, writer, StatusCode::new(rejection.status()), api::error(rejection), JSON, allow).await
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

/// The document as an event whenever what it shows changes (at most one per [`EVENT_INTERVAL`]),
/// and a comment every [`KEEPALIVE_INTERVAL`] that nothing else was sent.
struct Updates<'a> {
    slot: StreamSlot<'a>,
    event: &'static str,
}

impl EventSource for Updates<'_> {
    async fn write_events<W: picoserve::io::Write>(self, mut writer: EventWriter<'_, W>) -> Result<(), W::Error> {
        let shared = self.slot.0;
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
                    writer.write_event(self.event, document.as_str()).await?;
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
