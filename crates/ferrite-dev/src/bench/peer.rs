//! The desktop's end of the conversation: lines and raw bytes over one link, with deadlines.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ferrite_wire::bench::{Marker, READY_LINE};

use super::report::target_summary;
use super::{BenchError, Phase};
use crate::emit::Emitter;
use crate::tty::is_idle;

pub const READY_RESEND: Duration = Duration::from_secs(2);

pub struct Peer<'a, L> {
    link: &'a mut L,
    emitter: &'a mut Emitter,
    stop: &'a AtomicBool,
    pending: Vec<u8>,
    resend: Duration,
    /// The target's own `bench <phase>:` summary lines, without the prefix.
    pub summaries: BTreeMap<Phase, String>,
}

impl<'a, L: Read + Write> Peer<'a, L> {
    pub fn new(link: &'a mut L, emitter: &'a mut Emitter, stop: &'a AtomicBool) -> Self {
        Self { link, emitter, stop, pending: Vec::new(), resend: READY_RESEND, summaries: BTreeMap::new() }
    }

    #[cfg(test)]
    #[must_use]
    pub const fn resend_ready_every(mut self, every: Duration) -> Self {
        self.resend = every;
        self
    }

    pub fn event(&mut self, text: &str) {
        self.emitter.line(text);
    }

    pub fn check_stop(&self) -> Result<(), BenchError> {
        if self.stop.load(Ordering::Relaxed) { Err(BenchError::Stopped) } else { Ok(()) }
    }

    /// Bytes read past the last line taken, such as the start of a frame stream.
    pub fn take_pending(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }

    pub fn restore_pending(&mut self, bytes: Vec<u8>) {
        self.pending = bytes;
    }

    /// Read what is available into `buf`; `None` when the link was idle for one slice.
    pub fn read_into(&mut self, buf: &mut [u8]) -> Result<Option<usize>, BenchError> {
        match self.link.read(buf) {
            Ok(0) => Err(BenchError::Closed),
            Ok(n) => Ok(Some(n)),
            Err(e) if is_idle(&e) || e.kind() == io::ErrorKind::Interrupted => Ok(None),
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Err(BenchError::Closed),
            Err(source) => Err(BenchError::Io { source }),
        }
    }

    /// Write everything, retrying while the link is full, until `give_up`. Counts the retries.
    pub fn send_all(&mut self, mut bytes: &[u8], stalls: &mut u32, give_up: Instant) -> Result<(), BenchError> {
        while !bytes.is_empty() {
            match self.link.write(bytes) {
                Ok(0) => return Err(BenchError::Closed),
                Ok(n) => bytes = &bytes[n..],
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {},
                Err(e) if is_idle(&e) => {
                    *stalls += 1;
                    self.check_stop()?;
                    if Instant::now() >= give_up {
                        return Err(BenchError::Timeout { waiting_for: "the link to accept data".into() });
                    }
                },
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Err(BenchError::Closed),
                Err(source) => return Err(BenchError::Io { source }),
            }
        }
        Ok(())
    }

    fn send_ready(&mut self) -> Result<(), BenchError> {
        let mut line = READY_LINE.to_vec();
        line.push(b'\n');
        self.send_all(&line, &mut 0, Instant::now() + Duration::from_secs(5))
    }

    /// Send the ready line, resending it while the target is not listening yet, and return the
    /// next marker the target announces.
    pub fn begin(&mut self, what: &str, until: Instant) -> Result<Marker, BenchError> {
        self.send_ready()?;
        let mut resend_at = Instant::now() + self.resend;
        let mut buf = [0; 4096];
        loop {
            while let Some(line) = self.take_line() {
                if let Some(marker) = find_marker(&line) {
                    return Ok(marker);
                }
                self.note(&line);
            }
            self.check_stop()?;
            let now = Instant::now();
            if now >= until {
                return Err(BenchError::Timeout { waiting_for: format!("the target to announce {what}") });
            }
            if now >= resend_at {
                self.send_ready()?;
                resend_at = now + self.resend;
            }
            if let Some(n) = self.read_into(&mut buf)? {
                self.pending.extend_from_slice(&buf[..n]);
            }
        }
    }

    /// Read lines until `expected` arrives; any other marker is a protocol error.
    pub fn await_marker(&mut self, expected: Marker, until: Instant) -> Result<(), BenchError> {
        let mut buf = [0; 4096];
        loop {
            while let Some(line) = self.take_line() {
                match find_marker(&line) {
                    Some(marker) if marker == expected => return Ok(()),
                    Some(other) => {
                        return Err(BenchError::Unexpected { expected: expected.to_string(), got: other.to_string() });
                    },
                    None => self.note(&line),
                }
            }
            self.check_stop()?;
            if Instant::now() >= until {
                return Err(BenchError::Timeout { waiting_for: format!("`{expected}`") });
            }
            if let Some(n) = self.read_into(&mut buf)? {
                self.pending.extend_from_slice(&buf[..n]);
            }
        }
    }

    /// Keep reading lines until the target has reported every phase in `want` or `until` passes.
    pub fn collect_summaries(&mut self, want: &[Phase], until: Instant) -> Result<(), BenchError> {
        let mut buf = [0; 4096];
        while Instant::now() < until && !want.iter().all(|phase| self.summaries.contains_key(phase)) {
            self.check_stop()?;
            while let Some(line) = self.take_line() {
                self.note(&line);
            }
            match self.read_into(&mut buf) {
                Ok(Some(n)) => self.pending.extend_from_slice(&buf[..n]),
                Ok(None) => {},
                Err(BenchError::Closed) => break,
                Err(e) => return Err(e),
            }
        }
        while let Some(line) = self.take_line() {
            self.note(&line);
        }
        Ok(())
    }

    fn take_line(&mut self) -> Option<String> {
        let end = self.pending.iter().position(|&b| b == b'\n')?;
        let line: Vec<u8> = self.pending.drain(..=end).collect();
        Some(String::from_utf8_lossy(&line).trim().to_owned())
    }

    fn note(&mut self, line: &str) {
        if line.is_empty() {
            return;
        }
        if let Some((phase, summary)) = target_summary(line) {
            self.summaries.insert(phase, summary.to_owned());
        }
        self.emitter.line(&format!("target: {line}"));
    }
}

/// The marker in `line`, which may carry stray bytes in front of it.
pub fn find_marker(line: &str) -> Option<Marker> {
    Marker::parse(&line[line.find("BENCH ")?..])
}

#[cfg(test)]
mod tests {
    use assert2::check;
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::plain("BENCH sink 10", Some(Marker::Sink { secs: 10 }))]
    #[case::behind_garbage("\u{0}\u{1}\u{fffd}BENCH echo done", Some(Marker::EchoDone))]
    #[case::ready("BENCH ready", None)]
    #[case::summary_is_lowercase("bench sink: bytes=1", None)]
    #[case::none("hello", None)]
    fn markers_are_found_in_a_line(#[case] line: &str, #[case] want: Option<Marker>) {
        check!(find_marker(line) == want);
    }
}
