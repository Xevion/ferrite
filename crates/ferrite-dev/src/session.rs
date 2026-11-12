//! Running a [`Conversation`] over a real link: connect, send what the machine asks for, read what
//! comes back, show it, and reconnect where the conversation allows.

use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::thread;
use std::time::{Duration, Instant};

use ferrite_wire::link::{Decoder, EncodeError, MAX_PAYLOAD, Message, Stream, encode};
use snafu::{ResultExt, Snafu};

use crate::config::Config;
use crate::connect::{ConnectError, Connector, Waited};
use crate::conversation::{Conversation, ConversationError, Effect, Outcome, Stage};
use crate::emit::Emitter;
use crate::link::{Link, LinkOptions, Transport};
use crate::tty::is_idle;
use crate::usb::{self, RealUsb};

/// Messages sent before the link is read again, so a rejection is noticed within a few hundred KiB.
const SEND_BURST: usize = 32;
/// How long one message may wait for a link that is not taking writes.
const WRITE_PATIENCE: Duration = Duration::from_secs(10);
/// How long `Reboot` is repeated while the device is still there.
pub const REBOOT_CONFIRM: Duration = Duration::from_secs(20);
const REBOOT_RESEND: Duration = Duration::from_millis(500);
/// How long a device that the link has lost may take to leave the bus.
const GONE_WITHIN: Duration = Duration::from_secs(5);
const POLL_EVERY: Duration = Duration::from_millis(100);

#[derive(Debug, Snafu)]
pub enum SessionError {
    #[snafu(display("cannot connect to the DbC link"))]
    Connect { source: ConnectError },

    #[snafu(transparent)]
    Conversation { source: ConversationError },

    #[snafu(display("stopped before the conversation finished"))]
    Stopped,

    #[snafu(display("a message cannot be encoded"))]
    Encode { source: EncodeError },

    #[snafu(display("the DbC device was still present {}s after Reboot was sent", after.as_secs()))]
    RebootUnconfirmed { after: Duration },
}

/// Where a conversation's output goes: log lines to the emitter, the app's data stream to `data`.
pub struct Console {
    pub emitter: Emitter,
    data: Box<dyn Write>,
}

impl Console {
    pub fn new(emitter: Emitter, data: impl Write + 'static) -> Self {
        Self { emitter, data: Box::new(data) }
    }

    /// Log lines to stderr and `out`, the data stream to stdout.
    pub fn stdio(out: Option<&Path>) -> io::Result<Self> {
        let emitter = Emitter::new().with_sink(io::stderr());
        let emitter = match out {
            Some(path) => emitter.with_file(path)?,
            None => emitter,
        };
        Ok(Self::new(emitter, io::stdout()))
    }

    pub fn show(&mut self, effect: Effect) {
        match effect {
            Effect::Ready(info) => {
                self.emitter.line(&format!("--- loader ready: {} (protocol {}) ---", info.build, info.proto));
            },
            Effect::Accepted => self.emitter.line("--- push accepted ---"),
            Effect::Started(name) => self.emitter.line(&format!("--- started {name} ---")),
            Effect::Output { stream: Stream::Log, bytes } => {
                // One record per message and no newline of its own; a record may hold several lines.
                String::from_utf8_lossy(&bytes).lines().for_each(|line| self.emitter.line(line));
            },
            Effect::Output { stream: Stream::Data, bytes } => {
                if let Err(e) = self.data.write_all(&bytes).and_then(|()| self.data.flush()) {
                    self.emitter.line(&format!("--- cannot write the data stream to stdout: {e} ---"));
                }
            },
            Effect::Output { stream: Stream::Unknown(code), bytes } => {
                self.emitter.line(&format!("--- {} bytes on unknown output stream {code} ---", bytes.len()));
            },
            Effect::Fault(text) => self.emitter.line(&format!("--- ERROR loader fault: {text} ---")),
            Effect::Dropped(bytes) => self.emitter.line(&format!("--- {bytes} bytes of noise skipped on the link ---")),
            Effect::Note(note) => self.emitter.line(&format!("--- {note} ---")),
        }
    }

    fn show_all(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            self.show(effect);
        }
    }
}

/// How the DbC device is reached and how to tell whether it is still on the bus.
pub struct Dialer<'a> {
    config: &'a Config,
    transport: Transport,
    connector: Connector<'a>,
}

impl<'a> Dialer<'a> {
    pub fn new(config: &'a Config, options: LinkOptions) -> Self {
        let mut connector = Connector::new(config);
        if options.transport == Transport::Usb {
            connector = connector.usb(RealUsb::new(options.usb));
        }
        Self { config, transport: options.transport, connector }
    }

    /// Whether the DbC device is on the bus, whoever holds it.
    pub fn present(&self) -> bool {
        match self.transport {
            Transport::Usb => usb::is_present(),
            Transport::Tty => self.config.device.exists(),
        }
    }
}

/// What ended one connection.
#[derive(Debug, PartialEq, Eq)]
pub enum Drive {
    /// The conversation has a result.
    Finished,
    /// The link dropped; the conversation may go on over a new one.
    LinkLost,
}

/// Run `machine` until it has a result, reconnecting while the conversation allows. The link
/// that carried it to the end comes back with the outcome.
pub fn converse(
    machine: &mut Conversation,
    dialer: &mut Dialer<'_>,
    console: &mut Console,
    input: Option<&Receiver<Vec<u8>>>,
    stop: &AtomicBool,
) -> Result<(Outcome, Link), SessionError> {
    loop {
        match dialer.connector.wait(&mut console.emitter, machine.deadline(), stop).context(ConnectSnafu)? {
            Waited::Connected(mut link) => {
                let driven = drive(machine, &mut link, console, input, &|| dialer.present(), stop)?;
                if let Some(result) = machine.take_result() {
                    return Ok((result?, link));
                }
                if driven == Drive::LinkLost {
                    console.emitter.line("--- link down ---");
                    dialer.connector.link_lost();
                }
            },
            Waited::TimedOut => {
                machine.tick(Instant::now());
                if let Some(Err(error)) = machine.take_result() {
                    return Err(error.into());
                }
            },
            Waited::Stopped => return Err(SessionError::Stopped),
        }
    }
}

/// Play `machine` over one connection until it has a result or the link is lost.
pub fn drive(
    machine: &mut Conversation,
    link: &mut Link,
    console: &mut Console,
    input: Option<&Receiver<Vec<u8>>>,
    present: &dyn Fn() -> bool,
    stop: &AtomicBool,
) -> Result<Drive, SessionError> {
    let mut decoder = Decoder::new();
    let mut frame = Vec::new();
    let mut buf = vec![0; MAX_PAYLOAD];
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err(SessionError::Stopped);
        }
        machine.tick(Instant::now());
        console.show_all(machine.take_effects());
        if machine.is_finished() {
            return Ok(Drive::Finished);
        }

        if let Err(e) = send_due(machine, link, input, &mut frame) {
            match e {
                SendError::Encode(source) => return Err(SessionError::Encode { source }),
                SendError::Io(e) => return Ok(lost(machine, console, &e.to_string())),
            }
        }

        match link.read(&mut buf) {
            Ok(0) => return Ok(lost(machine, console, "the far side closed it")),
            Ok(n) => {
                let (now, machine) = (Instant::now(), &mut *machine);
                decoder.feed(&buf[..n], |event| machine.on_event(event, now));
            },
            Err(e) if is_idle(&e) => {
                if !present() {
                    return Ok(lost(machine, console, "the device is gone"));
                }
            },
            Err(e) => return Ok(lost(machine, console, &e.to_string())),
        }
    }
}

fn lost(machine: &mut Conversation, console: &mut Console, why: &str) -> Drive {
    console.emitter.line(&format!("--- link lost: {why} ---"));
    machine.link_lost(Instant::now());
    Drive::LinkLost
}

enum SendError {
    Encode(EncodeError),
    Io(io::Error),
}

impl From<io::Error> for SendError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Send what the machine has due, and the user's typed input while an app runs.
fn send_due(
    machine: &mut Conversation,
    link: &mut Link,
    input: Option<&Receiver<Vec<u8>>>,
    frame: &mut Vec<u8>,
) -> Result<(), SendError> {
    for _ in 0..SEND_BURST {
        let Some(msg) = machine.next_message(Instant::now()) else { break };
        send(link, &msg, frame)?;
    }
    if machine.stage() == Stage::Running {
        while let Some(bytes) = input.and_then(|rx| rx.try_recv().ok()) {
            for chunk in bytes.chunks(MAX_PAYLOAD) {
                send(link, &Message::Input { bytes: chunk.to_vec() }, frame)?;
            }
        }
    }
    link.flush()?;
    Ok(())
}

fn send(link: &mut Link, msg: &Message, frame: &mut Vec<u8>) -> Result<(), SendError> {
    frame.clear();
    encode(msg, frame).map_err(SendError::Encode)?;
    let deadline = Instant::now() + WRITE_PATIENCE;
    let mut rest = frame.as_slice();
    while !rest.is_empty() {
        match link.write(rest) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero).into()),
            Ok(n) => rest = &rest[n..],
            Err(e) if is_idle(&e) && Instant::now() < deadline => {},
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Send `Reboot` until the link or the device goes away, then check that the device really left
/// the bus, which is what the target resetting looks like from here.
pub fn reboot(
    link: &mut Link,
    dialer: &Dialer<'_>,
    console: &mut Console,
    stop: &AtomicBool,
) -> Result<(), SessionError> {
    let started = Instant::now();
    console.emitter.line("--- sending Reboot and waiting for the DbC device to disappear ---");
    let mut frame = Vec::new();
    let mut buf = vec![0; MAX_PAYLOAD];
    let mut next_send = started;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err(SessionError::Stopped);
        }
        if started.elapsed() >= REBOOT_CONFIRM {
            return Err(SessionError::RebootUnconfirmed { after: started.elapsed() });
        }
        if Instant::now() >= next_send {
            next_send = Instant::now() + REBOOT_RESEND;
            if let Err(SendError::Io(_)) = send(link, &Message::Reboot, &mut frame).and_then(|()| Ok(link.flush()?)) {
                break;
            }
        }
        match link.read(&mut buf) {
            Ok(0) => break,
            Ok(_) => {},
            Err(e) if is_idle(&e) => {
                if !dialer.present() {
                    break;
                }
            },
            Err(_) => break,
        }
    }
    let left = Instant::now() + GONE_WITHIN;
    while dialer.present() {
        if Instant::now() >= left {
            return Err(SessionError::RebootUnconfirmed { after: started.elapsed() });
        }
        thread::sleep(POLL_EVERY);
    }
    console.emitter.line(&format!(
        "--- the DbC device is gone {:.1}s after Reboot: the target is resetting ---",
        started.elapsed().as_secs_f32()
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::mpsc;

    use assert2::{assert, check};
    use ferrite_wire::link::{Event, Exited, PROTO, PUSH_CHUNK, PushKind, ShortStr};

    use super::*;
    use crate::conversation::{Goal, PushPlan, Timeouts};
    use crate::emit::tests::Shared;
    use crate::testing::FakeLink;

    const STATUS_ABORTED: u64 = 0x8000_0000_0000_0015;

    fn short(text: &str) -> ShortStr {
        ShortStr::new(text).expect("short enough")
    }

    fn ready() -> Message {
        Message::Ready { proto: PROTO, build: short("fake loader") }
    }

    fn timeouts() -> Timeouts {
        Timeouts { hello_every: Duration::from_millis(5), ..Timeouts::with_ready(Duration::from_secs(3)) }
    }

    /// A loader on the far end of a [`FakeLink`]: `behave` answers each message it decodes, and
    /// everything it was sent is kept in the returned log.
    fn fake_loader(
        mut behave: impl FnMut(&Message) -> Vec<Message> + 'static,
    ) -> (FakeLink, Rc<RefCell<Vec<Message>>>) {
        let received = Rc::new(RefCell::new(Vec::new()));
        let log = Rc::clone(&received);
        let mut decoder = Decoder::new();
        let link = FakeLink::new().responding(move |written| {
            let mut replies = Vec::new();
            decoder.feed(written, |event| {
                if let Event::Message(msg) = event {
                    for reply in behave(&msg) {
                        assert!(let Ok(()) = encode(&reply, &mut replies));
                    }
                    log.borrow_mut().push(msg);
                }
            });
            if replies.is_empty() { Vec::new() } else { vec![(Duration::ZERO, replies)] }
        });
        (link, received)
    }

    struct Harness {
        log: Shared,
        data: Shared,
        console: Console,
    }

    impl Harness {
        fn new() -> Self {
            let (log, data) = (Shared::default(), Shared::default());
            let emitter = Emitter::new().with_sink(log.clone()).with_stamp(|| "T".into());
            Self { console: Console::new(emitter, data.clone()), log, data }
        }

        fn drive(
            &mut self,
            machine: &mut Conversation,
            link: FakeLink,
            input: Option<&Receiver<Vec<u8>>>,
        ) -> Result<Drive, SessionError> {
            let mut link = Link::new(Transport::Usb, "fake", link);
            drive(machine, &mut link, &mut self.console, input, &|| true, &AtomicBool::new(false))
        }
    }

    fn push_machine(kind: PushKind, image: Vec<u8>) -> Conversation {
        let plan = PushPlan::new(kind, short("app.efi"), image, 60);
        Conversation::new(Goal::Push(Box::new(plan)), timeouts(), Instant::now())
    }

    fn image(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 253) as u8).collect()
    }

    /// Answers like the real loader for a push that is accepted and whose app does `after_start`.
    fn accepting(
        mut after_start: impl FnMut(&Message) -> Vec<Message> + 'static,
        end_replies: Vec<Message>,
    ) -> impl FnMut(&Message) -> Vec<Message> + 'static {
        move |msg| match msg {
            Message::Hello { .. } => vec![ready()],
            Message::PushBegin(_) => vec![Message::PushAccepted],
            Message::PushEnd => end_replies.clone(),
            other => after_start(other),
        }
    }

    #[test]
    fn hello_is_repeated_until_the_loader_finally_answers() {
        let mut hellos = 0;
        let (link, received) = fake_loader(move |msg| match msg {
            Message::Hello { .. } => {
                hellos += 1;
                if hellos == 3 { vec![ready()] } else { Vec::new() }
            },
            _ => Vec::new(),
        });
        let mut machine = Conversation::new(Goal::Handshake, timeouts(), Instant::now());
        let mut harness = Harness::new();
        assert!(let Ok(Drive::Finished) = harness.drive(&mut machine, link, None));
        assert!(let Some(Ok(Outcome::Ready(info))) = machine.take_result());
        check!(info.build.as_str() == "fake loader");
        check!(received.borrow().iter().filter(|m| matches!(m, Message::Hello { .. })).count() >= 3);
        check!(
            harness.log.text().contains("T --- loader ready: fake loader (protocol 1) ---\n"),
            "{}",
            harness.log.text()
        );
    }

    #[test]
    fn a_loader_of_another_protocol_is_refused_without_a_push() {
        let (link, received) = fake_loader(|_| vec![Message::Ready { proto: PROTO + 1, build: short("future") }]);
        let mut machine = push_machine(PushKind::App, image(10));
        assert!(let Ok(Drive::Finished) = Harness::new().drive(&mut machine, link, None));
        check!(machine.take_result() == Some(Err(ConversationError::ProtoMismatch { ours: PROTO, theirs: PROTO + 1 })));
        check!(received.borrow().iter().all(|m| matches!(m, Message::Hello { .. })));
    }

    #[test]
    fn a_reject_at_begin_ends_the_push_before_any_data() {
        let (link, received) = fake_loader(|msg| match msg {
            Message::Hello { .. } => vec![ready()],
            Message::PushBegin(_) => vec![Message::PushRejected { reason: "busy: an app is running".into() }],
            _ => Vec::new(),
        });
        let mut machine = push_machine(PushKind::App, image(PUSH_CHUNK * 2));
        assert!(let Ok(Drive::Finished) = Harness::new().drive(&mut machine, link, None));
        assert!(let Some(Err(ConversationError::Rejected { after_end: false, .. })) = machine.take_result());
        check!(!received.borrow().iter().any(|m| matches!(m, Message::PushData { .. } | Message::PushEnd)));
    }

    #[test]
    fn a_reject_after_the_end_arrives_once_the_whole_image_went_out() {
        let bytes = image(PUSH_CHUNK + 10);
        let behave = accepting(
            |_| Vec::new(),
            vec![Message::PushRejected { reason: "the firmware refused the image: LOAD_ERROR".into() }],
        );
        let (link, received) = fake_loader(behave);
        let mut machine = push_machine(PushKind::App, bytes.clone());
        assert!(let Ok(Drive::Finished) = Harness::new().drive(&mut machine, link, None));
        assert!(let Some(Err(ConversationError::Rejected { after_end: true, reason })) = machine.take_result());
        check!(reason.contains("LOAD_ERROR"));

        let received_image: Vec<u8> = received
            .borrow()
            .iter()
            .filter_map(|m| if let Message::PushData { bytes, .. } = m { Some(bytes.clone()) } else { None })
            .flatten()
            .collect();
        check!(received_image == bytes);
        check!(matches!(received.borrow().last(), Some(Message::PushEnd)));
    }

    #[test]
    fn a_full_push_streams_log_lines_and_data_and_ends_with_a_failing_status() {
        let end = vec![
            Message::Started { name: short("app.efi") },
            Message::Output { stream: Stream::Log, bytes: b"[    0.001] hello\n[    0.002] two".to_vec() },
            Message::Output { stream: Stream::Data, bytes: vec![0, 1, 2, 0x7F, b'\n'] },
            Message::Exited(Exited::new(STATUS_ABORTED, Duration::from_millis(1500))),
        ];
        let (link, _) = fake_loader(accepting(|_| Vec::new(), end));
        let mut machine = push_machine(PushKind::App, image(100));
        let mut harness = Harness::new();
        assert!(let Ok(Drive::Finished) = harness.drive(&mut machine, link, None));
        assert!(let Some(Ok(Outcome::Exited(exited))) = machine.take_result());
        check!((exited.status, exited.elapsed_ms) == (STATUS_ABORTED, 1500));
        let log = harness.log.text();
        check!(log.contains("T --- started app.efi ---\nT [    0.001] hello\nT [    0.002] two\n"), "{log}");
        check!(harness.data.text().as_bytes() == [0, 1, 2, 0x7F, b'\n'], "data is raw bytes, not lines");
    }

    #[test]
    fn typed_lines_reach_the_app_only_after_it_has_started() {
        let behave = accepting(
            |msg| match msg {
                Message::Input { bytes } => vec![
                    Message::Output { stream: Stream::Data, bytes: bytes.clone() },
                    Message::Exited(Exited::new(0, Duration::ZERO)),
                ],
                _ => Vec::new(),
            },
            vec![Message::Started { name: short("app.efi") }],
        );
        let (link, received) = fake_loader(behave);
        let (tx, rx) = mpsc::channel();
        tx.send(b"ping\n".to_vec()).expect("receiver alive");
        let mut machine = push_machine(PushKind::App, image(10));
        let mut harness = Harness::new();
        assert!(let Ok(Drive::Finished) = harness.drive(&mut machine, link, Some(&rx)));
        assert!(let Some(Ok(Outcome::Exited(_))) = machine.take_result());
        check!(harness.data.text() == "ping\n");
        let received = received.borrow();
        let end = received.iter().position(|m| matches!(m, Message::PushEnd)).expect("PushEnd sent");
        let input = received.iter().position(|m| matches!(m, Message::Input { .. })).expect("Input sent");
        check!(end < input);
    }

    #[test]
    fn a_loader_push_is_done_when_the_new_loader_says_ready() {
        let behave = accepting(|_| Vec::new(), vec![Message::Started { name: short("app.efi") }, ready()]);
        let (link, _) = fake_loader(behave);
        let mut machine = push_machine(PushKind::Loader, image(10));
        assert!(let Ok(Drive::Finished) = Harness::new().drive(&mut machine, link, None));
        assert!(let Some(Ok(Outcome::Replaced(_))) = machine.take_result());
    }

    #[test]
    fn a_closed_link_before_ready_is_a_loss_the_conversation_survives() {
        let mut machine = Conversation::new(Goal::Handshake, timeouts(), Instant::now());
        let mut harness = Harness::new();
        assert!(let Ok(Drive::LinkLost) = harness.drive(&mut machine, FakeLink::new().closing(), None));
        check!(!machine.is_finished());
        check!(harness.log.text().contains("--- link lost: the far side closed it ---"));
    }
}
