//! `log`: timestamped capture of everything the target prints over the DbC link.
//!
//! A standalone app writes plain text, which is cut into lines. While the resident loader runs, the
//! stream is `ferrite_wire::link` frames instead: log records become lines (the line ending is
//! added here, a record has none) and data goes to stdout unchanged. The two are told apart by
//! the frame sync, `FE 4C`, which plain text never contains.

use std::borrow::Cow;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ferrite_wire::link::{Decoder, Event, Message, SYNC, Stream};
use snafu::{ResultExt, Snafu};

use crate::config::Config;
use crate::connect::{ConnectError, Connector, Waited};
use crate::efi_status::EfiStatus;
use crate::emit::{Emitter, LineBuffer};
use crate::tty::is_idle;

pub const PROBE_DELAY: Duration = Duration::from_secs(4);

#[derive(Debug, Snafu)]
pub enum LogError {
    #[snafu(display("cannot connect to the DbC link"))]
    Connect { source: ConnectError },
}

#[derive(Debug, Clone, Default)]
pub struct LogOptions {
    /// Stop after this long; `None` captures until stopped.
    pub duration: Option<Duration>,
    /// One line sent to the target a while after each connect.
    pub probe: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum Pumped {
    LinkLost,
    Finished,
}

/// Capture until the duration passes or `stop` is set, reconnecting whenever the link drops.
pub fn run(config: &Config, options: &LogOptions, emitter: &mut Emitter, stop: &AtomicBool) -> Result<(), LogError> {
    let deadline = options.duration.map(|d| Instant::now() + d);
    emitter.line(
        &options
            .duration
            .map_or_else(|| "--- capture start ---".to_owned(), |d| format!("--- capture start, {}s ---", d.as_secs())),
    );
    let mut connector = Connector::new(config);
    let mut data = io::stdout();
    while let Waited::Connected(mut tty) = connector.wait(emitter, deadline, stop).context(ConnectSnafu)? {
        let present = || config.device.exists();
        let mut shown = Shown { emitter, data: &mut data };
        if pump(&mut tty, options, PROBE_DELAY, &present, deadline, stop, &mut shown) == Pumped::Finished {
            break;
        }
        emitter.line("--- link down ---");
        connector.link_lost();
    }
    emitter.line("--- capture end ---");
    Ok(())
}

/// Where a capture goes: stamped lines to the emitter, data frames raw to `data`.
struct Shown<'a> {
    emitter: &'a mut Emitter,
    data: &'a mut dyn Write,
}

/// Cuts the byte stream into lines until link framing shows up, and into frames from then on.
#[derive(Default)]
struct Capture {
    lines: LineBuffer,
    decoder: Decoder,
    framed: bool,
    /// A trailing `FE` that may be the first half of a sync split across reads.
    held: Option<u8>,
}

impl Capture {
    fn push(&mut self, bytes: &[u8], shown: &mut Shown<'_>) {
        if self.framed {
            self.decode(bytes, shown);
            return;
        }
        let joined = self.held.take().map_or(Cow::Borrowed(bytes), |held| Cow::Owned([&[held][..], bytes].concat()));
        if let Some(at) = joined.windows(SYNC.len()).position(|pair| pair == SYNC) {
            self.lines.push(&joined[..at], |line| shown.emitter.line(line));
            self.lines.finish(|line| shown.emitter.line(line));
            self.framed = true;
            self.decode(&joined[at..], shown);
            return;
        }
        let text = match joined.split_last() {
            Some((&last, rest)) if last == SYNC[0] => {
                self.held = Some(last);
                rest
            },
            _ => &joined,
        };
        self.lines.push(text, |line| shown.emitter.line(line));
    }

    /// The target went quiet or the link closed: show a final line that never got its newline.
    fn finish(&mut self, shown: &mut Shown<'_>) {
        self.lines.push(self.held.take().as_slice(), |line| shown.emitter.line(line));
        self.lines.finish(|line| shown.emitter.line(line));
    }

    fn decode(&mut self, bytes: &[u8], shown: &mut Shown<'_>) {
        self.decoder.feed(bytes, |event| show(event, shown));
    }
}

fn show(event: Event, shown: &mut Shown<'_>) {
    let emitter = &mut *shown.emitter;
    match event {
        Event::Message(Message::Output { stream: Stream::Log, bytes }) => emitter.record(&bytes),
        Event::Message(Message::Output { stream: Stream::Data, bytes }) => {
            if let Err(e) = shown.data.write_all(&bytes).and_then(|()| shown.data.flush()) {
                emitter.line(&format!("--- cannot write the data stream to stdout: {e} ---"));
            }
        },
        Event::Message(Message::Output { stream: Stream::Unknown(code), bytes }) => {
            emitter.line(&format!("--- {} bytes on unknown output stream {code} ---", bytes.len()));
        },
        Event::Message(Message::Ready { proto, identity, build }) => {
            emitter.line(&format!("--- loader ready: {build} (protocol {proto}, {identity}) ---"));
        },
        Event::Message(Message::Started { name }) => emitter.line(&format!("--- started {name} ---")),
        Event::Message(Message::Exited(exited)) => emitter.line(&format!(
            "--- exited: {} after {:.3}s ---",
            EfiStatus::from_code(exited.status),
            exited.elapsed().as_secs_f32()
        )),
        Event::Message(Message::PushRejected { reason }) => emitter.line(&format!("--- push rejected: {reason} ---")),
        Event::Message(Message::Fault { text }) => emitter.line(&format!("--- ERROR loader fault: {text} ---")),
        Event::Message(_) => {},
        Event::Dropped { bytes } => emitter.line(&format!("--- {bytes} bytes of noise skipped on the link ---")),
        Event::Malformed { ty, error } => {
            emitter.line(&format!("--- a malformed {ty:?} frame from the loader: {error} ---"));
        },
    }
}

fn pump<L: Read + Write>(
    link: &mut L,
    options: &LogOptions,
    probe_delay: Duration,
    present: &dyn Fn() -> bool,
    deadline: Option<Instant>,
    stop: &AtomicBool,
    shown: &mut Shown<'_>,
) -> Pumped {
    let connected = Instant::now();
    let mut probe = options.probe.as_deref();
    let mut capture = Capture::default();
    let mut buf = [0; 4096];
    let ended = loop {
        if stop.load(Ordering::Relaxed) || deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            break Pumped::Finished;
        }
        if let Some(text) = probe.filter(|_| connected.elapsed() >= probe_delay) {
            probe = None;
            if let Err(e) = link.write_all(format!("{text}\n").as_bytes()) {
                shown.emitter.line(&format!("--- probe not sent: {e} ---"));
            }
        }
        match link.read(&mut buf) {
            Ok(0) => break Pumped::LinkLost,
            Ok(n) => capture.push(&buf[..n], shown),
            Err(e) if is_idle(&e) => {
                if !present() {
                    break Pumped::LinkLost;
                }
            },
            Err(_) => break Pumped::LinkLost,
        }
    };
    capture.finish(shown);
    ended
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};
    use ferrite_wire::link::{Exited, Identity, PROTO, ShortStr, encode};

    use super::*;
    use crate::emit::tests::Shared;
    use crate::testing::FakeLink;

    fn emitter(out: &Shared) -> Emitter {
        Emitter::new().with_sink(out.clone()).with_stamp(|| "T".into())
    }

    /// Pump `link` to its end, with data going to `data`.
    fn pump_to(link: &mut FakeLink, options: &LogOptions, out: &Shared, data: &Shared, present: bool) -> Pumped {
        let deadline = Some(Instant::now() + Duration::from_secs(5));
        let (mut emitter, mut data) = (emitter(out), data.clone());
        let mut shown = Shown { emitter: &mut emitter, data: &mut data };
        pump(link, options, Duration::ZERO, &|| present, deadline, &AtomicBool::new(false), &mut shown)
    }

    fn pump_once(link: &mut FakeLink, options: &LogOptions, out: &Shared, present: bool) -> Pumped {
        pump_to(link, options, out, &Shared::default(), present)
    }

    fn frames(msgs: &[Message]) -> Vec<u8> {
        let mut out = Vec::new();
        for msg in msgs {
            assert!(let Ok(()) = encode(msg, &mut out));
        }
        out
    }

    fn log_record(text: &str) -> Message {
        Message::Output { stream: Stream::Log, bytes: text.as_bytes().to_vec() }
    }

    #[test]
    fn lines_are_stamped_and_the_unterminated_tail_is_kept_when_the_link_closes() {
        let mut link = FakeLink::new().with_rx(b"boot\r\nline two\npart").max_read(5).closing();
        let out = Shared::default();
        let ended = pump_once(&mut link, &LogOptions::default(), &out, true);
        check!(ended == Pumped::LinkLost);
        check!(out.text() == "T boot\nT line two\nT part\n");
    }

    #[test]
    fn the_probe_goes_out_once_after_the_delay() {
        let mut link = FakeLink::new().with_rx(b"hi\n").closing();
        let options = LogOptions { duration: None, probe: Some("probe-from-host".into()) };
        pump_once(&mut link, &options, &Shared::default(), true);
        check!(link.written == b"probe-from-host\n");
    }

    #[test]
    fn no_probe_unless_asked() {
        let mut link = FakeLink::new().with_rx(b"hi\n").closing();
        pump_once(&mut link, &LogOptions::default(), &Shared::default(), true);
        check!(link.written.is_empty());
    }

    #[test]
    fn a_vanished_device_ends_an_idle_link() {
        let mut link = FakeLink::new();
        check!(pump_once(&mut link, &LogOptions::default(), &Shared::default(), false) == Pumped::LinkLost);
    }

    #[test]
    fn stop_finishes_without_reporting_a_loss() {
        let mut link = FakeLink::new();
        let out = Shared::default();
        let (mut emitter, mut data) = (emitter(&out), Shared::default());
        let mut shown = Shown { emitter: &mut emitter, data: &mut data };
        let ended =
            pump(&mut link, &LogOptions::default(), PROBE_DELAY, &|| true, None, &AtomicBool::new(true), &mut shown);
        check!(ended == Pumped::Finished);
    }

    #[test]
    fn link_frames_become_lines_and_raw_data_while_the_loader_is_resident() {
        let ready = Message::Ready {
            proto: PROTO,
            identity: Identity { boot: 1, instance: 0xAB },
            build: ShortStr::new("ferrite-loader").expect("short"),
        };
        let stream = frames(&[
            ready,
            log_record("[    0.001] hello"),
            Message::Output { stream: Stream::Data, bytes: vec![0, 0xFE, 0x4C, b'\n'] },
            log_record("[    0.002] two"),
            Message::Exited(Exited::new(0, Duration::from_millis(1500))),
        ]);
        let mut link = FakeLink::new().with_rx(&stream).max_read(7).closing();
        let (out, data) = (Shared::default(), Shared::default());
        pump_to(&mut link, &LogOptions::default(), &out, &data, true);
        check!(
            out.text()
                == "T --- loader ready: ferrite-loader (protocol 2, boot 1, instance 00000000000000ab) ---\n\
                    T [    0.001] hello\n\
                    T [    0.002] two\n\
                    T --- exited: EFI_SUCCESS (0) after 1.500s ---\n",
            "{}",
            out.text()
        );
        check!(data.bytes() == [0, 0xFE, 0x4C, b'\n'], "data is raw bytes, not lines");
    }

    #[test]
    fn text_before_the_first_frame_stays_text_and_a_lone_fe_does_not_switch_modes() {
        let mut bytes = b"plain \xFE text\nlast words".to_vec();
        bytes.extend(frames(&[log_record("framed")]));
        let mut link = FakeLink::new().with_rx(&bytes).max_read(1).closing();
        let out = Shared::default();
        pump_once(&mut link, &LogOptions::default(), &out, true);
        check!(out.text() == "T plain \u{FFFD} text\nT last words\nT framed\n", "{}", out.text());
    }
}
