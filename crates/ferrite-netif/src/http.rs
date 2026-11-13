//! A minimal HTTP/1.0 responder: one TCP socket on a port answers every request with `200 OK` and a
//! plain-text body, then closes.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use smoltcp::iface::{SocketHandle, SocketSet};
use smoltcp::socket::tcp;

const BUFFER: usize = 4096;
/// A request whose headers do not end within this many bytes is answered anyway.
const REQUEST_MAX: usize = 2048;

pub struct HttpResponder {
    handle: SocketHandle,
    port: u16,
    request: Vec<u8>,
    answered: bool,
    served: u32,
}

impl HttpResponder {
    pub fn new(sockets: &mut SocketSet<'static>, port: u16) -> Self {
        let socket = tcp::Socket::new(tcp::SocketBuffer::new(vec![0; BUFFER]), tcp::SocketBuffer::new(vec![0; BUFFER]));
        Self { handle: sockets.add(socket), port, request: Vec::new(), answered: false, served: 0 }
    }

    /// Listen again after a connection ends, and answer a complete request with `body()`.
    pub fn poll(&mut self, sockets: &mut SocketSet<'static>, body: impl FnOnce() -> String) {
        let socket = sockets.get_mut::<tcp::Socket<'_>>(self.handle);
        if !socket.is_open() {
            self.request.clear();
            self.answered = false;
            if let Err(error) = socket.listen(self.port) {
                log::warn!("http: listening on :{} failed: {error}", self.port);
            }
            return;
        }
        if self.answered {
            return;
        }
        while socket.can_recv() {
            let request = &mut self.request;
            let taken = socket.recv(|data| {
                let room = REQUEST_MAX.saturating_sub(request.len()).min(data.len());
                request.extend_from_slice(&data[..room]);
                (data.len(), ())
            });
            if taken.is_err() {
                break;
            }
        }
        let ended = self.request.windows(4).any(|w| w == b"\r\n\r\n") || self.request.len() >= REQUEST_MAX;
        if !ended || !socket.can_send() {
            return;
        }
        let body = body();
        let head = alloc::format!(
            "HTTP/1.0 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let line = self.request.split(|b| *b == b'\r').next().unwrap_or_default();
        let from = socket.remote_endpoint().map_or_else(|| String::from("?"), |at| alloc::format!("{at}"));
        log::info!("http: \"{}\" from {from}", String::from_utf8_lossy(line));
        for part in [head.as_bytes(), body.as_bytes()] {
            if socket.send_slice(part).map_or(true, |sent| sent < part.len()) {
                log::warn!("http: the response did not fit the send buffer");
            }
        }
        socket.close();
        self.answered = true;
        self.served += 1;
    }

    /// Requests answered.
    pub const fn served(&self) -> u32 {
        self.served
    }
}
