//! picoserve's socket and timer traits over the smoltcp TCP sockets of the HTTP server.
//!
//! Every operation looks at the socket once and returns `Pending` when it cannot proceed. Nothing
//! registers a waker: the server polls every connection future on each network poll.

use core::future::{Future, poll_fn, ready};
use core::task::Poll;
use core::time::Duration;

use picoserve::io::{BaseWrite, ErrorKind, ErrorType, Read, ReadExt as _, Write};
use picoserve::mem::{BorrowedBuffer, BorrowedCursor};
use picoserve::{Timeouts, Timer};
use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp;
use smoltcp::wire::IpEndpoint;
use snafu::Snafu;

use super::Shared;

/// The runtime the traits are implemented for: futures polled from the network loop.
pub struct Polled;

#[derive(Debug, Snafu)]
pub enum SocketError {
    #[snafu(display("connection closed for sending"))]
    Closed,
    #[snafu(display("receive failed: {reason}"))]
    Recv { reason: tcp::RecvError },
    #[snafu(display("send failed: {reason}"))]
    Send { reason: tcp::SendError },
}

impl picoserve::io::Error for SocketError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Closed => ErrorKind::ConnectionReset,
            Self::Recv { .. } | Self::Send { .. } => ErrorKind::Other,
        }
    }
}

/// One server socket, as both halves of a connection.
#[derive(Clone, Copy)]
pub struct Conn<'a> {
    shared: &'a Shared,
    handle: SocketHandle,
}

impl<'a> Conn<'a> {
    pub const fn new(shared: &'a Shared, handle: SocketHandle) -> Self {
        Self { shared, handle }
    }

    fn with<R>(self, f: impl FnOnce(&mut tcp::Socket<'static>) -> R) -> R {
        f(self.shared.sockets.borrow_mut().get_mut::<tcp::Socket<'static>>(self.handle))
    }

    pub fn remote(self) -> Option<IpEndpoint> {
        self.with(|sock| sock.remote_endpoint())
    }

    /// Listen on `port` until a client connects.
    pub async fn accept(self, port: u16) {
        let mut warned = false;
        poll_fn(|_| {
            self.with(|sock| match sock.state() {
                tcp::State::Closed | tcp::State::TimeWait => {
                    if let Err(e) = sock.listen(port)
                        && !warned
                    {
                        warned = true;
                        log::warn!("http: listen on :{port} failed: {e:?}");
                    }
                    Poll::Pending
                },
                tcp::State::Listen | tcp::State::SynReceived => Poll::Pending,
                _ => Poll::Ready(()),
            })
        })
        .await;
    }

    /// Wait until the peer has acknowledged everything sent, or the connection is gone.
    pub async fn drained(self) {
        poll_fn(|_| {
            self.with(|sock| if sock.send_queue() == 0 || !sock.is_open() { Poll::Ready(()) } else { Poll::Pending })
        })
        .await;
    }

    /// Give a closing connection until `linger` to finish on its own, then abort it, so the socket
    /// can listen again.
    pub async fn settle(self, linger: Duration) {
        let deadline = self.shared.now() + linger;
        poll_fn(|_| {
            self.with(|sock| {
                if !sock.is_open() {
                    Poll::Ready(())
                } else if self.shared.now() >= deadline {
                    log::debug!("http: aborting {:?}, still {} after {linger:?}", sock.remote_endpoint(), sock.state());
                    sock.abort();
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
        })
        .await;
    }
}

impl ErrorType for Conn<'_> {
    type Error = SocketError;
}

impl Read for Conn<'_> {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, SocketError> {
        if buf.is_empty() {
            return Ok(0);
        }
        poll_fn(|_| {
            self.with(|sock| {
                if sock.can_recv() {
                    Poll::Ready(match sock.recv_slice(buf) {
                        Ok(n) => Ok(n),
                        Err(tcp::RecvError::Finished) => Ok(0),
                        Err(reason) => Err(SocketError::Recv { reason }),
                    })
                } else if sock.may_recv() {
                    Poll::Pending
                } else {
                    Poll::Ready(Ok(0))
                }
            })
        })
        .await
    }
}

impl BaseWrite for Conn<'_> {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, SocketError> {
        if buf.is_empty() {
            return Ok(0);
        }
        poll_fn(|_| {
            self.with(|sock| {
                if sock.can_send() {
                    Poll::Ready(sock.send_slice(buf).map_err(|reason| SocketError::Send { reason }))
                } else if sock.may_send() {
                    Poll::Pending
                } else {
                    Poll::Ready(Err(SocketError::Closed))
                }
            })
        })
        .await
    }

    /// Queued data goes out at the end of this network poll and smoltcp retransmits it, so only a
    /// dead connection is reported. Waiting for the ACK instead would stall every body behind the
    /// head picoserve flushes first, by however long the peer delays its ACK.
    fn flush(&mut self) -> impl Future<Output = Result<(), SocketError>> {
        let dead = self.with(|sock| sock.send_queue() > 0 && !sock.is_open());
        ready(if dead { Err(SocketError::Closed) } else { Ok(()) })
    }
}

impl Write for Conn<'_> {
    async fn write_with<F: FnOnce(BorrowedCursor<'_>) -> R, R>(&mut self, f: F) -> Result<R, SocketError> {
        let mut f = Some(f);
        poll_fn(|_| {
            self.with(|sock| {
                if !sock.may_send() {
                    return Poll::Ready(Err(SocketError::Closed));
                }
                if !sock.can_send() {
                    return Poll::Pending;
                }
                let Some(f) = f.take() else {
                    return Poll::Pending;
                };
                let sent = sock.send(|window| {
                    let mut buf = BorrowedBuffer::new(window);
                    let out = f(buf.unfilled());
                    (buf.len(), out)
                });
                Poll::Ready(sent.map_err(|reason| SocketError::Send { reason }))
            })
        })
        .await
    }
}

impl picoserve::io::Socket<Polled> for Conn<'_> {
    type Error = SocketError;
    type ReadHalf<'b>
        = Self
    where
        Self: 'b;
    type WriteHalf<'b>
        = Self
    where
        Self: 'b;

    fn split(&mut self) -> (Self, Self) {
        (*self, *self)
    }

    fn abort<T: Timer<Polled>>(
        self,
        _timeouts: &Timeouts,
        _timer: &T,
    ) -> impl Future<Output = Result<(), picoserve::Error<SocketError>>> {
        self.with(tcp::Socket::abort);
        ready(Ok(()))
    }

    /// Send our FIN, read until the client closes its side, then wait for the last data to be
    /// acknowledged, as picoserve's embassy-net socket does.
    async fn shutdown<T: Timer<Polled>>(
        mut self,
        timeouts: &Timeouts,
        timer: &T,
    ) -> Result<(), picoserve::Error<SocketError>> {
        self.with(tcp::Socket::close);
        timer
            .run_with_timeout(timeouts.read_request, self.discard_all_data())
            .await
            .map_err(picoserve::Error::ReadTimeout)?
            .map_err(picoserve::Error::Read)?;
        timer.run_with_timeout(timeouts.write, self.drained()).await.map_err(picoserve::Error::WriteTimeout)
    }
}

/// Time as of the current network poll.
#[derive(Clone, Copy)]
pub struct Clock<'a>(pub &'a Shared);

impl Clock<'_> {
    pub fn sleep(self, duration: Duration) -> impl Future<Output = ()> {
        let shared = self.0;
        let deadline = shared.now() + duration;
        poll_fn(move |_| if shared.now() >= deadline { Poll::Ready(()) } else { Poll::Pending })
    }
}

impl Timer<Polled> for Clock<'_> {
    fn delay(&self, duration: picoserve::time::Duration) -> impl Future<Output = ()> {
        self.sleep(Duration::from_millis(duration.as_millis()))
    }
}
