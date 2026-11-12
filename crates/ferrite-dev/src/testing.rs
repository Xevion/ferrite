//! An in-memory stand-in for the DbC tty.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::thread;
use std::time::{Duration, Instant};

/// Bytes the far side sends back after a write, each readable after its delay.
pub type Replies = Vec<(Duration, Vec<u8>)>;

type Responder = Box<dyn FnMut(&[u8]) -> Replies>;

/// Bytes queued for reading, a record of everything written, and an optional peer that reacts to
/// each write. With nothing readable, reads are idle until the link is made to `closing`.
pub struct FakeLink {
    rx: VecDeque<u8>,
    pub written: Vec<u8>,
    closed: bool,
    max_read: usize,
    responder: Option<Responder>,
    deferred: Vec<(Instant, Vec<u8>)>,
}

impl FakeLink {
    pub fn new() -> Self {
        Self {
            rx: VecDeque::new(),
            written: Vec::new(),
            closed: false,
            max_read: usize::MAX,
            responder: None,
            deferred: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_rx(mut self, bytes: &[u8]) -> Self {
        self.rx.extend(bytes);
        self
    }

    /// Bytes that become readable only after `delay`, such as an end marker.
    #[must_use]
    pub fn with_rx_after(mut self, delay: Duration, bytes: &[u8]) -> Self {
        self.deferred.push((Instant::now() + delay, bytes.to_vec()));
        self
    }

    /// Reads end with `Ok(0)` once nothing is queued.
    #[must_use]
    pub const fn closing(mut self) -> Self {
        self.closed = true;
        self
    }

    /// Return at most `n` bytes per read, as a real link cuts a stream at arbitrary points.
    #[must_use]
    pub const fn max_read(mut self, n: usize) -> Self {
        self.max_read = n;
        self
    }

    #[must_use]
    pub fn responding(mut self, responder: impl FnMut(&[u8]) -> Replies + 'static) -> Self {
        self.responder = Some(Box::new(responder));
        self
    }
}

impl Read for FakeLink {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let now = Instant::now();
        let (due, later): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.deferred).into_iter().partition(|(at, _)| *at <= now);
        self.deferred = later;
        for (_, bytes) in due {
            self.rx.extend(bytes);
        }
        if self.rx.is_empty() {
            if self.closed {
                return Ok(0);
            }
            thread::sleep(Duration::from_millis(1));
            return Err(io::ErrorKind::TimedOut.into());
        }
        let n = buf.len().min(self.max_read).min(self.rx.len());
        for (slot, byte) in buf.iter_mut().zip(self.rx.drain(..n)) {
            *slot = byte;
        }
        Ok(n)
    }
}

impl Write for FakeLink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.written.extend_from_slice(buf);
        if let Some(responder) = &mut self.responder {
            let now = Instant::now();
            self.deferred.extend(responder(buf).into_iter().map(|(delay, bytes)| (now + delay, bytes)));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
