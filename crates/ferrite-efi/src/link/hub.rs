//! The core of the loader link: framing, queues and routing between the DbC byte stream and the
//! two sides that use it. It makes no firmware calls, so it runs on the host.
//!
//! The loader's own code and the apps it runs share one hub. Output from either is framed and
//! queued here; bytes from the desktop are decoded here and routed by [`Mode`].

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use ferrite_wire::link::{
    CRC_LEN, Decoder, EncodeError, Event, HEADER_LEN, Identity, MAX_OUTPUT, Message, PROTO, ShortStr, Stream, encode,
    encode_output,
};
use snafu::Snafu;

/// Encoded bytes waiting for the transport; output past this is refused.
pub const OUT_CAP: usize = 256 * 1024;
/// Extra room for control messages, so a full queue cannot swallow `Ready` or `Exited`.
const CONTROL_RESERVE: usize = 16 * 1024;
/// Bytes from the desktop that wait for the running app's `read`.
pub const INBOX_CAP: usize = 16 * 1024;
/// Decoded messages for the loader's own code that may wait unread.
const COMMAND_BYTES_CAP: usize = 8 * 1024 * 1024;

/// The byte pipe under the hub: the DbC in the loader, a script in tests.
pub trait Transport {
    /// Service the link and hand every received byte run to `rx`.
    fn poll_rx(&mut self, rx: impl FnMut(&[u8]));
    /// Queue bytes for the far end and return how many were taken.
    fn send(&mut self, data: &[u8]) -> usize;
    /// The far end is attached and configured.
    fn is_up(&self) -> bool;
    /// Bytes taken by [`Transport::send`] and not yet confirmed delivered.
    fn backlog(&self) -> usize;
    /// A short name for the link state, for status lines.
    fn status(&self) -> &'static str;
    /// A multi-line account of the link's internals, for a link that delivers late or not at all.
    fn diagnostics(&mut self) -> Option<String> {
        None
    }
}

/// Who receives what the desktop sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// No app is running: pushes and aborts go to the loader's own code.
    Idle,
    /// An app is running: input and `Stop` go to it and a new push is refused.
    Running,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HubStats {
    /// Bytes the decoder skipped looking for a frame.
    pub rx_dropped: u64,
    /// Frames with a good CRC and a bad payload.
    pub malformed: u32,
    /// Messages meant for the other direction, or ignored in the current mode.
    pub ignored: u32,
    /// Frames refused because the out queue was full.
    pub out_refused: u32,
    /// Input bytes dropped: no app running, or its inbox was full.
    pub input_dropped: u64,
    /// Messages for the loader dropped because too many were waiting.
    pub commands_dropped: u32,
    /// `Hello`s answered: each desktop session starts with at least one.
    pub hellos: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Snafu)]
pub enum SendError {
    #[snafu(display("the out queue is full"))]
    Full,
    #[snafu(display("the message cannot be encoded"))]
    Encode { source: EncodeError },
}

pub struct Hub<T> {
    transport: T,
    build: ShortStr,
    identity: Identity,
    decoder: Decoder,
    out: VecDeque<u8>,
    scratch: Vec<u8>,
    commands: VecDeque<Message>,
    command_bytes: usize,
    inbox: VecDeque<u8>,
    mode: Mode,
    was_up: bool,
    reboot: bool,
    stop: bool,
    stats: HubStats,
}

impl<T: Transport> Hub<T> {
    /// `build` and `identity` are announced in every `Ready`.
    pub fn new(transport: T, build: ShortStr, identity: Identity) -> Self {
        Self {
            transport,
            build,
            identity,
            decoder: Decoder::new(),
            out: VecDeque::new(),
            scratch: Vec::new(),
            commands: VecDeque::new(),
            command_bytes: 0,
            inbox: VecDeque::new(),
            mode: Mode::Idle,
            was_up: false,
            reboot: false,
            stop: false,
            stats: HubStats::default(),
        }
    }

    pub fn into_transport(self) -> T {
        self.transport
    }

    pub const fn transport(&self) -> &T {
        &self.transport
    }

    pub const fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    pub const fn stats(&self) -> HubStats {
        self.stats
    }

    pub const fn mode(&self) -> Mode {
        self.mode
    }

    /// Switching clears whatever input was waiting and any pending `Stop`.
    pub fn set_mode(&mut self, mode: Mode) {
        if self.mode != mode {
            self.mode = mode;
            self.inbox.clear();
            self.stop = false;
        }
    }

    /// The desktop asked the running app to finish. Stays set until the mode changes, so every
    /// look at it gives the same answer.
    pub const fn stop_requested(&self) -> bool {
        self.stop
    }

    /// The desktop asked for a reset. Sticky: the caller acts on it.
    pub const fn reboot_requested(&self) -> bool {
        self.reboot
    }

    /// Encoded bytes not yet confirmed delivered, queued here or inside the transport.
    pub fn pending(&self) -> usize {
        self.out.len() + self.transport.backlog()
    }

    /// Service the transport, route what arrived, announce a (re)configured link and push queued
    /// bytes out.
    pub fn pump(&mut self) {
        self.flush();
        let mut events = Vec::new();
        let Self { transport, decoder, .. } = self;
        transport.poll_rx(|bytes| decoder.feed(bytes, |event| events.push(event)));

        let up = self.transport.is_up();
        if up && !self.was_up {
            self.announce();
        }
        self.was_up = up;
        for event in events {
            self.route(event);
        }
        self.flush();
    }

    /// Queue a message for the desktop and push it toward the transport.
    pub fn send(&mut self, msg: &Message) -> Result<(), SendError> {
        let queued = self.queue(msg);
        self.flush();
        queued
    }

    /// Frame `bytes` as output on `stream`, all or nothing.
    ///
    /// Refused with [`SendError::Full`] when the queue cannot hold every frame, so a frame is
    /// never cut. Call [`Hub::flush`] to push the queue toward the transport.
    pub fn output(&mut self, stream: Stream, bytes: &[u8]) -> Result<(), SendError> {
        self.scratch.clear();
        if let Err(e) = frame_output(stream, bytes, OUT_CAP.saturating_sub(self.out.len()), &mut self.scratch) {
            self.stats.out_refused = self.stats.out_refused.saturating_add(1);
            return Err(e);
        }
        self.out.extend(self.scratch.drain(..));
        Ok(())
    }

    /// Take frames another context encoded while the hub was busy.
    pub fn absorb(&mut self, frames: &[u8]) {
        if frames.is_empty() {
            return;
        }
        if self.out.len() + frames.len() > OUT_CAP + CONTROL_RESERVE {
            self.stats.out_refused = self.stats.out_refused.saturating_add(1);
            return;
        }
        self.out.extend(frames);
    }

    /// Hand queued bytes to the transport until it stops taking them.
    pub fn flush(&mut self) {
        while !self.out.is_empty() {
            let taken = self.transport.send(self.out.as_slices().0);
            if taken == 0 {
                return;
            }
            self.out.drain(..taken);
        }
    }

    /// The oldest message for the loader's own code.
    pub fn next_command(&mut self) -> Option<Message> {
        let msg = self.commands.pop_front()?;
        self.command_bytes -= command_cost(&msg);
        Some(msg)
    }

    /// Bytes the desktop sent for the running app, oldest first; 0 when nothing is waiting.
    pub fn read_input(&mut self, buf: &mut [u8]) -> usize {
        let n = buf.len().min(self.inbox.len());
        for (slot, byte) in buf.iter_mut().zip(self.inbox.drain(..n)) {
            *slot = byte;
        }
        n
    }

    fn announce(&mut self) {
        let ready = Message::Ready { proto: PROTO, identity: self.identity, build: self.build.clone() };
        // A full queue already holds far more than the desktop will read; the next Hello asks again.
        let _ = self.queue(&ready);
    }

    fn queue(&mut self, msg: &Message) -> Result<(), SendError> {
        self.scratch.clear();
        if let Err(source) = encode(msg, &mut self.scratch) {
            return Err(SendError::Encode { source });
        }
        if self.out.len() + self.scratch.len() > OUT_CAP + CONTROL_RESERVE {
            self.stats.out_refused = self.stats.out_refused.saturating_add(1);
            return Err(SendError::Full);
        }
        self.out.extend(self.scratch.drain(..));
        Ok(())
    }

    fn route(&mut self, event: Event) {
        let msg = match event {
            Event::Dropped { bytes } => {
                self.stats.rx_dropped += bytes as u64;
                return;
            },
            Event::Malformed { ty, error } => {
                self.stats.malformed += 1;
                let text = format!("malformed {ty:?} message: {error}");
                let _ = self.queue(&Message::Fault { text });
                return;
            },
            Event::Message(msg) => msg,
        };
        match msg {
            Message::Hello { .. } => {
                self.stats.hellos = self.stats.hellos.wrapping_add(1);
                self.announce();
            },
            Message::Reboot => self.reboot = true,
            Message::Stop => match self.mode {
                Mode::Running => self.stop = true,
                Mode::Idle => self.stats.ignored += 1,
            },
            Message::Input { bytes } => self.deliver_input(&bytes),
            Message::Unknown { code, len } => {
                let text = format!("unknown message type {code:#04x} ({len} bytes)");
                let _ = self.queue(&Message::Fault { text });
            },
            Message::PushBegin(_) | Message::PushData { .. } | Message::PushEnd | Message::Abort => match self.mode {
                Mode::Idle => self.command(msg),
                Mode::Running if matches!(msg, Message::PushBegin(_)) => {
                    let reason = String::from("busy: an app is running");
                    let _ = self.queue(&Message::PushRejected { reason });
                },
                Mode::Running => self.stats.ignored += 1,
            },
            _ => self.stats.ignored += 1,
        }
    }

    fn deliver_input(&mut self, bytes: &[u8]) {
        let room = match self.mode {
            Mode::Running => INBOX_CAP - self.inbox.len(),
            Mode::Idle => 0,
        };
        let take = room.min(bytes.len());
        self.inbox.extend(&bytes[..take]);
        self.stats.input_dropped += (bytes.len() - take) as u64;
    }

    fn command(&mut self, msg: Message) {
        let cost = command_cost(&msg);
        if self.command_bytes + cost > COMMAND_BYTES_CAP {
            self.stats.commands_dropped += 1;
            return;
        }
        self.command_bytes += cost;
        self.commands.push_back(msg);
    }
}

fn command_cost(msg: &Message) -> usize {
    64 + match msg {
        Message::PushData { bytes, .. } => bytes.len(),
        _ => 0,
    }
}

/// Encode `bytes` as output frames of at most [`MAX_OUTPUT`] bytes onto `into`, if all of them
/// fit in `room` bytes; otherwise write nothing.
pub fn frame_output(stream: Stream, bytes: &[u8], room: usize, into: &mut Vec<u8>) -> Result<(), SendError> {
    let frames = bytes.len().div_ceil(MAX_OUTPUT);
    let total = bytes.len() + frames * (HEADER_LEN + 1 + CRC_LEN);
    if total > room {
        return Err(SendError::Full);
    }
    for chunk in bytes.chunks(MAX_OUTPUT) {
        encode_output(stream, chunk, into).map_err(|source| SendError::Encode { source })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use assert2::{assert, check};
    use ferrite_wire::link::{PushBegin, PushKind};

    use super::*;

    #[derive(Default)]
    struct Script {
        incoming: VecDeque<Vec<u8>>,
        sent: Vec<u8>,
        per_send: Option<usize>,
        up: bool,
    }

    impl Transport for Script {
        fn poll_rx(&mut self, mut rx: impl FnMut(&[u8])) {
            while let Some(bytes) = self.incoming.pop_front() {
                rx(&bytes);
            }
        }

        fn send(&mut self, data: &[u8]) -> usize {
            let n = self.per_send.map_or(data.len(), |limit| limit.min(data.len()));
            self.sent.extend_from_slice(&data[..n]);
            n
        }

        fn is_up(&self) -> bool {
            self.up
        }

        fn backlog(&self) -> usize {
            0
        }

        fn status(&self) -> &'static str {
            "scripted"
        }
    }

    fn hub() -> Hub<Script> {
        Hub::new(Script { up: true, ..Script::default() }, ShortStr::new("test build").expect("short"), IDENTITY)
    }

    fn deliver(hub: &mut Hub<Script>, msgs: &[Message]) {
        let mut bytes = Vec::new();
        for msg in msgs {
            assert!(let Ok(()) = encode(msg, &mut bytes));
        }
        hub.transport.incoming.push_back(bytes);
        hub.pump();
    }

    fn sent(hub: &mut Hub<Script>) -> Vec<Message> {
        let mut out = Vec::new();
        Decoder::new().feed(&core::mem::take(&mut hub.transport.sent), |event| {
            if let Event::Message(msg) = event {
                out.push(msg);
            }
        });
        out
    }

    fn begin() -> Message {
        Message::PushBegin(PushBegin {
            kind: PushKind::App,
            size: 4,
            crc32: 0,
            watchdog_secs: 60,
            name: ShortStr::new("a.efi").expect("short"),
        })
    }

    const IDENTITY: Identity = Identity { boot: 2, instance: 0xABCD };

    fn ready() -> Message {
        Message::Ready { proto: PROTO, identity: IDENTITY, build: ShortStr::new("test build").expect("short") }
    }

    #[test]
    fn ready_is_sent_each_time_the_link_comes_up_and_on_hello() {
        let mut hub = hub();
        hub.pump();
        check!(sent(&mut hub) == vec![ready()]);
        hub.pump();
        check!(sent(&mut hub).is_empty(), "steady state is quiet");

        hub.transport.up = false;
        hub.pump();
        hub.transport.up = true;
        hub.pump();
        check!(sent(&mut hub) == vec![ready()], "a reconfigure announces again");

        deliver(&mut hub, &[Message::Hello { proto: PROTO }]);
        check!(sent(&mut hub) == vec![ready()]);
        hub.set_mode(Mode::Running);
        deliver(&mut hub, &[Message::Hello { proto: PROTO }]);
        check!(sent(&mut hub) == vec![ready()], "also while an app runs");
    }

    #[test]
    fn idle_routes_push_messages_to_the_loader_in_order() {
        let mut hub = hub();
        let data = Message::PushData { offset: 0, bytes: vec![1, 2, 3, 4] };
        deliver(&mut hub, &[begin(), data.clone(), Message::PushEnd, Message::Abort]);
        check!(hub.next_command() == Some(begin()));
        check!(hub.next_command() == Some(data));
        check!(hub.next_command() == Some(Message::PushEnd));
        check!(hub.next_command() == Some(Message::Abort));
        check!(hub.next_command() == None);
    }

    #[test]
    fn a_running_app_gets_input_and_refuses_pushes() {
        let mut hub = hub();
        hub.set_mode(Mode::Running);
        deliver(
            &mut hub,
            &[
                Message::Input { bytes: b"abc".to_vec() },
                begin(),
                Message::PushData { offset: 0, bytes: vec![0] },
                Message::Abort,
            ],
        );
        check!(hub.next_command() == None);
        let mut buf = [0; 8];
        check!(hub.read_input(&mut buf) == 3);
        check!(&buf[..3] == b"abc");
        check!(hub.read_input(&mut buf) == 0);
        let replies = sent(&mut hub);
        assert!(let [Message::Ready { .. }, Message::PushRejected { reason }] = replies.as_slice());
        check!(reason.contains("busy"));
        check!(hub.stats().ignored == 2);
    }

    #[test]
    fn input_is_bounded_dropped_when_idle_and_cleared_on_a_mode_change() {
        let mut hub = hub();
        deliver(&mut hub, &[Message::Input { bytes: vec![1; 10] }]);
        check!(hub.stats().input_dropped == 10, "nothing is reading");

        hub.set_mode(Mode::Running);
        deliver(&mut hub, &[Message::Input { bytes: vec![2; INBOX_CAP + 5] }]);
        check!(hub.stats().input_dropped == 15);
        hub.set_mode(Mode::Idle);
        hub.set_mode(Mode::Running);
        check!(hub.read_input(&mut [0; 4]) == 0);
    }

    #[test]
    fn stop_reaches_a_running_app_and_stays_until_the_run_ends() {
        let mut hub = hub();
        check!(!hub.stop_requested());
        hub.set_mode(Mode::Running);
        deliver(&mut hub, &[Message::Input { bytes: b"x".to_vec() }, Message::Stop]);
        check!(hub.stop_requested());
        check!(hub.stop_requested(), "looking does not consume it");
        check!(hub.next_command() == None, "it is not a command for the loader");
        let mut buf = [0; 4];
        check!(hub.read_input(&mut buf) == 1, "input is still delivered in order with it");

        hub.set_mode(Mode::Idle);
        check!(!hub.stop_requested(), "the next run does not inherit it");
    }

    #[test]
    fn stop_with_no_app_running_is_ignored_and_does_not_arm_the_next_run() {
        let mut hub = hub();
        deliver(&mut hub, &[Message::Stop]);
        check!(!hub.stop_requested());
        check!(hub.stats().ignored == 1);
        hub.set_mode(Mode::Running);
        check!(!hub.stop_requested());
    }

    #[test]
    fn reboot_is_never_a_command_in_any_mode() {
        let mut hub = hub();
        check!(!hub.reboot_requested());
        hub.set_mode(Mode::Running);
        deliver(&mut hub, &[Message::Reboot]);
        check!(hub.reboot_requested());
        check!(hub.next_command() == None);
    }

    #[test]
    fn a_bad_payload_and_an_unknown_type_are_answered_with_a_fault() {
        let mut hub = hub();
        let mut bytes = Vec::new();
        let body = [&[0x01u8][..], &1u32.to_le_bytes(), &[9]].concat();
        bytes.extend_from_slice(&ferrite_wire::link::SYNC);
        bytes.extend_from_slice(&body);
        bytes.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
        hub.transport.incoming.push_back(bytes);
        deliver(&mut hub, &[]);
        let replies = sent(&mut hub);
        assert!(let [Message::Ready { .. }, Message::Fault { text }] = replies.as_slice());
        check!(text.contains("malformed"));
        check!(hub.stats().malformed == 1);
    }

    #[test]
    fn output_is_framed_in_chunks_and_decodes_back() {
        let mut hub = hub();
        let bytes: Vec<u8> = (0..MAX_OUTPUT + 10).map(|i| i as u8).collect();
        assert!(let Ok(()) = hub.output(Stream::Data, &bytes));
        hub.flush();
        let frames = sent(&mut hub);
        assert!(let [Message::Output { bytes: first, .. }, Message::Output { bytes: second, .. }] = frames.as_slice());
        check!(first.len() == MAX_OUTPUT && second.len() == 10);
        check!([first.as_slice(), second.as_slice()].concat() == bytes);
    }

    #[test]
    fn a_full_queue_refuses_whole_writes_and_keeps_control_messages() {
        let mut hub = hub();
        hub.transport.per_send = Some(0);
        let block = vec![7; 60 * 1024];
        let mut accepted = 0;
        while hub.output(Stream::Log, &block).is_ok() {
            accepted += 1;
        }
        check!(accepted == 4, "256 KiB holds four 60 KiB frames");
        let queued = hub.out.len();
        check!(hub.output(Stream::Log, &block) == Err(SendError::Full));
        check!(hub.out.len() == queued, "a refused write queues nothing");
        check!(hub.send(&Message::PushAccepted) == Ok(()), "control messages use the reserve");
        check!(hub.stats().out_refused >= 2);
    }

    #[test]
    fn a_transport_that_takes_a_few_bytes_at_a_time_keeps_frames_in_order() {
        let mut hub = hub();
        hub.transport.per_send = Some(5);
        assert!(let Ok(()) = hub.output(Stream::Log, b"first"));
        assert!(let Ok(()) = hub.output(Stream::Log, b"second"));
        hub.flush();
        check!(sent(&mut hub).len() == 2);
        check!(hub.pending() == 0);
    }

    #[test]
    fn frames_absorbed_from_another_context_are_sent_after_what_was_queued() {
        let mut hub = hub();
        assert!(let Ok(()) = hub.output(Stream::Log, b"one"));
        let mut parked = Vec::new();
        assert!(let Ok(()) = encode_output(Stream::Log, b"two", &mut parked));
        hub.absorb(&parked);
        hub.flush();
        let frames = sent(&mut hub);
        assert!(let [Message::Output { bytes: a, .. }, Message::Output { bytes: b, .. }] = frames.as_slice());
        check!((a.as_slice(), b.as_slice()) == (&b"one"[..], &b"two"[..]));
    }
}
