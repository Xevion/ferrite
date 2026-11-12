//! The desktop's half of the loader conversation as a state machine with no I/O.
//!
//! The driver feeds it decoder events, the current time and link losses, and asks it which message
//! to send next. Everything it wants shown to the user comes out as [`Effect`]s. The rules it
//! follows are the ones in `ferrite_wire::link`, checked against the loader's hub. Among them: a
//! push ends only in `Exited`, `PushRejected` or a replacement's `Ready`, and `Fault` never ends it.

use std::fmt;
use std::mem;
use std::time::{Duration, Instant};

use ferrite_wire::link::{Event, Exited, Identity, Message, PROTO, PUSH_CHUNK, PushBegin, PushKind, ShortStr, Stream};
use snafu::Snafu;

pub const HELLO_EVERY: Duration = Duration::from_millis(250);
/// How long `PushAccepted` and `Started` get, which only cost the loader a copy, a CRC and `LoadImage`.
pub const REPLY_WITHIN: Duration = Duration::from_secs(15);
/// How long past its watchdog an app gets to report before the desktop gives up waiting.
pub const WATCHDOG_GRACE: Duration = Duration::from_secs(30);
/// How long a replacement loader gets to bring the link up again.
pub const REPLACEMENT_WITHIN: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timeouts {
    pub hello_every: Duration,
    /// How long the loader gets to answer `Hello` with `Ready`.
    pub ready: Duration,
    pub reply: Duration,
    pub grace: Duration,
    pub replacement: Duration,
}

impl Timeouts {
    #[must_use]
    pub const fn with_ready(ready: Duration) -> Self {
        Self {
            hello_every: HELLO_EVERY,
            ready,
            reply: REPLY_WITHIN,
            grace: WATCHDOG_GRACE,
            replacement: REPLACEMENT_WITHIN,
        }
    }
}

/// An image and its announcement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushPlan {
    pub begin: PushBegin,
    pub image: Vec<u8>,
}

impl PushPlan {
    /// Size and CRC come from `image`.
    #[must_use]
    pub fn new(kind: PushKind, name: ShortStr, image: Vec<u8>, watchdog_secs: u32) -> Self {
        let begin = PushBegin { kind, size: image.len() as u64, crc32: crc32fast::hash(&image), watchdog_secs, name };
        Self { begin, image }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Goal {
    /// Only learn that a loader is there and speaks our protocol.
    Handshake,
    Push(Box<PushPlan>),
}

/// What the conversation is waiting for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Greeting,
    AwaitAccept,
    Sending,
    AwaitStart,
    Running,
    AwaitReplacement,
    Finished,
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Greeting => "the loader's Ready",
            Self::AwaitAccept => "the loader to accept the push",
            Self::Sending => "the image to go out",
            Self::AwaitStart => "the loader to start the image",
            Self::Running => "the app to exit",
            Self::AwaitReplacement => "the replacement loader's Ready",
            Self::Finished => "nothing",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadyInfo {
    pub proto: u16,
    pub identity: Identity,
    pub build: ShortStr,
}

/// What the desktop does about an interrupt (Ctrl-C).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interrupted {
    /// `Stop` is on its way to the running app; the conversation carries on until it exits.
    Asked,
    /// The app was already asked: leave it running and end the conversation.
    Leave,
    /// Nothing to ask: end the conversation.
    Exit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A loader answered (the goal was a handshake).
    Ready(ReadyInfo),
    /// The pushed app ran to its end.
    Exited(Exited),
    /// The pushed loader is up and said `Ready`.
    Replaced(ReadyInfo),
}

#[derive(Clone, Debug, PartialEq, Eq, Snafu)]
pub enum ConversationError {
    #[snafu(display("the loader speaks protocol {theirs} and this tool speaks {ours}"))]
    ProtoMismatch { ours: u16, theirs: u16 },

    #[snafu(display("the loader rejected the push{}: {reason}", if *after_end { " after the whole image was sent" } else { "" }))]
    Rejected { reason: String, after_end: bool },

    #[snafu(display("the replacement loader returned and the old one is serving again ({identity})"))]
    ReplacementReturned { identity: Identity },

    #[snafu(display("timed out after {:.1}s waiting for {waiting_for}", after.as_secs_f32()))]
    Timeout { waiting_for: Stage, after: Duration },

    #[snafu(display("the link was lost while waiting for {during}"))]
    LinkLost { during: Stage },
}

/// Something for the driver to show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    Ready(ReadyInfo),
    Accepted,
    Started(ShortStr),
    Output {
        stream: Stream,
        bytes: Vec<u8>,
    },
    Fault(String),
    /// Bytes the decoder skipped looking for the next frame.
    Dropped(usize),
    Note(String),
}

#[derive(Debug)]
pub struct Conversation {
    goal: Goal,
    timeouts: Timeouts,
    stage: Stage,
    entered: Instant,
    next_hello: Option<Instant>,
    begin_sent: bool,
    offset: usize,
    effects: Vec<Effect>,
    result: Option<Result<Outcome, ConversationError>>,
    greeting_timed_out: bool,
    /// Who answered the greeting, to tell a replacement loader from the old one.
    loader: Option<Identity>,
    /// When the loader last said anything while the app ran.
    heard: Instant,
    stop_after: Option<Duration>,
    stop: StopState,
}

/// Whether the running app has been asked to stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StopState {
    NotAsked,
    /// To be sent with the next message.
    Pending,
    Sent,
}

impl Conversation {
    #[must_use]
    pub const fn new(goal: Goal, timeouts: Timeouts, now: Instant) -> Self {
        Self {
            goal,
            timeouts,
            stage: Stage::Greeting,
            entered: now,
            next_hello: Some(now),
            begin_sent: false,
            offset: 0,
            effects: Vec::new(),
            result: None,
            greeting_timed_out: false,
            loader: None,
            heard: now,
            stop_after: None,
            stop: StopState::NotAsked,
        }
    }

    /// Send `Stop` once the app has run this long.
    #[must_use]
    pub const fn with_stop_after(mut self, after: Duration) -> Self {
        self.stop_after = Some(after);
        self
    }

    /// The user interrupted. While an app runs the first interrupt asks it to stop and a second
    /// gives up on it; at any other point there is nothing to ask and the run ends.
    pub fn interrupt(&mut self) -> Interrupted {
        match (self.stage, self.stop) {
            (Stage::Running, StopState::NotAsked) => {
                self.stop = StopState::Pending;
                self.effects.push(Effect::Note(
                    "interrupted: asking the app to stop (interrupt again to leave it running)".into(),
                ));
                Interrupted::Asked
            },
            (Stage::Running, StopState::Pending | StopState::Sent) => {
                self.effects.push(Effect::Note(
                    "interrupted again: leaving the app running on the target; its watchdog or `ferrite-dev loader reboot` ends it".into(),
                ));
                Interrupted::Leave
            },
            _ => Interrupted::Exit,
        }
    }

    #[must_use]
    pub const fn stage(&self) -> Stage {
        self.stage
    }

    /// What there is to show since the last call.
    pub fn take_effects(&mut self) -> Vec<Effect> {
        mem::take(&mut self.effects)
    }

    /// The end of the conversation, once there is one.
    pub fn take_result(&mut self) -> Option<Result<Outcome, ConversationError>> {
        self.result.take()
    }

    pub const fn is_finished(&self) -> bool {
        matches!(self.stage, Stage::Finished)
    }

    /// When the current stage runs out of time; `None` while sending, which the link paces itself.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        let limit = match self.stage {
            Stage::Greeting => self.timeouts.ready,
            Stage::AwaitAccept | Stage::AwaitStart => self.timeouts.reply,
            Stage::Running => return Some(self.heard + self.watchdog() + self.timeouts.grace),
            Stage::AwaitReplacement => self.timeouts.replacement,
            Stage::Sending | Stage::Finished => return None,
        };
        Some(self.entered + limit)
    }

    fn watchdog(&self) -> Duration {
        match &self.goal {
            Goal::Push(plan) => plan.begin.watchdog(),
            Goal::Handshake => Duration::ZERO,
        }
    }

    fn pushing_loader(&self) -> bool {
        matches!(&self.goal, Goal::Push(plan) if plan.begin.kind == PushKind::Loader)
    }

    fn enter(&mut self, stage: Stage, now: Instant) {
        self.stage = stage;
        self.entered = now;
        self.heard = now;
    }

    fn finish(&mut self, result: Result<Outcome, ConversationError>) {
        if self.result.is_none() && !self.is_finished() {
            self.stage = Stage::Finished;
            self.result = Some(result);
        }
    }

    /// End the conversation with a timeout if the stage's time is up.
    pub fn tick(&mut self, now: Instant) {
        if let Some(deadline) = self.deadline()
            && now >= deadline
        {
            self.greeting_timed_out = self.stage == Stage::Greeting;
            let error = ConversationError::Timeout {
                waiting_for: self.stage,
                after: now.saturating_duration_since(self.entered),
            };
            self.finish(Err(error));
        }
    }

    /// Give a conversation that ended waiting for `Ready` a new budget, after something outside
    /// it (booting the loader) changed what to expect. False when it ended any other way.
    pub fn restart_greeting(&mut self, now: Instant, ready: Duration) -> bool {
        if !mem::take(&mut self.greeting_timed_out) {
            return false;
        }
        self.result = None;
        self.timeouts.ready = ready;
        self.enter(Stage::Greeting, now);
        self.next_hello = Some(now);
        true
    }

    /// The link dropped. True when waiting on is still sensible: before `Ready`, or while a
    /// replacement loader brings the link back. Otherwise the conversation ends here.
    pub fn link_lost(&mut self, now: Instant) -> bool {
        match self.stage {
            Stage::Greeting | Stage::AwaitReplacement => {
                self.next_hello = Some(now);
                true
            },
            Stage::AwaitStart if self.pushing_loader() => {
                self.effects
                    .push(Effect::Note("the link dropped before Started arrived; taking it as the handover".into()));
                self.enter(Stage::AwaitReplacement, now);
                self.next_hello = Some(now);
                true
            },
            Stage::Finished => true,
            during => {
                self.finish(Err(ConversationError::LinkLost { during }));
                false
            },
        }
    }

    /// The next message for the loader, if one is due.
    pub fn next_message(&mut self, now: Instant) -> Option<Message> {
        match self.stage {
            Stage::Greeting | Stage::AwaitReplacement => {
                self.next_hello.filter(|&at| now >= at)?;
                self.next_hello = Some(now + self.timeouts.hello_every);
                Some(Message::Hello { proto: PROTO })
            },
            Stage::AwaitAccept => {
                let Goal::Push(plan) = &self.goal else { return None };
                if mem::replace(&mut self.begin_sent, true) {
                    return None;
                }
                Some(Message::PushBegin(plan.begin.clone()))
            },
            Stage::Sending => {
                let Goal::Push(plan) = &self.goal else { return None };
                if self.offset < plan.image.len() {
                    let end = (self.offset + PUSH_CHUNK).min(plan.image.len());
                    let bytes = plan.image[self.offset..end].to_vec();
                    let offset = self.offset as u64;
                    self.offset = end;
                    Some(Message::PushData { offset, bytes })
                } else {
                    self.enter(Stage::AwaitStart, now);
                    Some(Message::PushEnd)
                }
            },
            Stage::Running => {
                let due = self.stop_after.is_some_and(|after| now >= self.entered + after);
                if due && self.stop == StopState::NotAsked {
                    self.stop = StopState::Pending;
                    self.effects.push(Effect::Note("--stop-after reached: asking the app to stop".into()));
                }
                (self.stop == StopState::Pending).then(|| {
                    self.stop = StopState::Sent;
                    Message::Stop
                })
            },
            Stage::AwaitStart | Stage::Finished => None,
        }
    }

    pub fn on_event(&mut self, event: Event, now: Instant) {
        if self.is_finished() {
            return;
        }
        match event {
            Event::Dropped { bytes } => self.effects.push(Effect::Dropped(bytes)),
            Event::Malformed { ty, error } => {
                self.effects.push(Effect::Note(format!("a malformed {ty:?} frame from the loader: {error}")));
            },
            Event::Message(msg) => self.on_message(msg, now),
        }
    }

    fn on_message(&mut self, msg: Message, now: Instant) {
        if self.stage == Stage::Running {
            self.heard = now;
        }
        match (msg, self.stage) {
            (Message::Output { stream, bytes }, _) => self.effects.push(Effect::Output { stream, bytes }),
            (Message::Fault { text }, _) => self.effects.push(Effect::Fault(text)),
            (Message::Ready { proto, identity, build }, Stage::Greeting) => {
                self.ready(ReadyInfo { proto, identity, build }, now);
            },
            (Message::Ready { proto, identity, build }, Stage::AwaitReplacement) => {
                self.replacement_ready(ReadyInfo { proto, identity, build });
            },
            // Each Hello is answered, so replies to the repeats arrive after the first Ready.
            (Message::Ready { .. }, Stage::AwaitAccept) => {},
            (Message::PushAccepted, Stage::AwaitAccept) if self.begin_sent => {
                self.effects.push(Effect::Accepted);
                self.enter(Stage::Sending, now);
            },
            (
                Message::PushRejected { reason },
                stage @ (Stage::AwaitAccept | Stage::Sending | Stage::AwaitStart | Stage::AwaitReplacement),
            ) => {
                let after_end = matches!(stage, Stage::AwaitStart | Stage::AwaitReplacement);
                self.finish(Err(ConversationError::Rejected { reason, after_end }));
            },
            (Message::Started { name }, Stage::AwaitStart) => {
                self.effects.push(Effect::Started(name));
                let next = if self.pushing_loader() { Stage::AwaitReplacement } else { Stage::Running };
                self.enter(next, now);
            },
            (Message::Exited(exited), Stage::Running) => self.finish(Ok(Outcome::Exited(exited))),
            (other, stage) => {
                self.effects
                    .push(Effect::Note(format!("unexpected {:?} while waiting for {stage}", other.message_type())));
            },
        }
    }

    fn ready(&mut self, info: ReadyInfo, now: Instant) {
        if info.proto != PROTO {
            self.finish(Err(ConversationError::ProtoMismatch { ours: PROTO, theirs: info.proto }));
            return;
        }
        self.effects.push(Effect::Ready(info.clone()));
        self.loader = Some(info.identity);
        self.next_hello = None;
        match self.goal {
            Goal::Handshake => self.finish(Ok(Outcome::Ready(info))),
            Goal::Push(_) => self.enter(Stage::AwaitAccept, now),
        }
    }

    /// A `Ready` while a replacement is awaited: only a loader with another instance ID is it.
    fn replacement_ready(&mut self, info: ReadyInfo) {
        if info.proto != PROTO {
            self.finish(Err(ConversationError::ProtoMismatch { ours: PROTO, theirs: info.proto }));
            return;
        }
        match self.loader {
            Some(old) if old.instance == info.identity.instance => {
                if info.identity.boot > old.boot {
                    self.finish(Err(ConversationError::ReplacementReturned { identity: info.identity }));
                }
            },
            _ => self.finish(Ok(Outcome::Replaced(info))),
        }
    }
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};
    use ferrite_wire::link::{Decoder, encode};

    use super::*;

    const STATUS_ABORTED: u64 = 0x8000_0000_0000_0015;

    fn short(text: &str) -> ShortStr {
        ShortStr::new(text).expect("short enough")
    }

    const OLD: Identity = Identity { boot: 1, instance: 0xAAAA };
    const NEW: Identity = Identity { boot: 1, instance: 0xBBBB };

    fn ready(proto: u16) -> Message {
        ready_as(proto, OLD)
    }

    fn ready_as(proto: u16, identity: Identity) -> Message {
        Message::Ready { proto, identity, build: short("ferrite-loader test") }
    }

    fn plan(kind: PushKind, len: usize) -> PushPlan {
        let image = (0..len).map(|i| (i % 251) as u8).collect();
        PushPlan::new(kind, short("app.efi"), image, 60)
    }

    struct Rig {
        machine: Conversation,
        now: Instant,
    }

    impl Rig {
        fn new(goal: Goal) -> Self {
            let now = Instant::now();
            Self { machine: Conversation::new(goal, Timeouts::with_ready(Duration::from_secs(5)), now), now }
        }

        fn push(kind: PushKind, len: usize) -> Self {
            Self::new(Goal::Push(Box::new(plan(kind, len))))
        }

        fn advance(&mut self, by: Duration) {
            self.now += by;
        }

        fn hear(&mut self, msg: Message) {
            self.machine.on_event(Event::Message(msg), self.now);
        }

        /// Feed raw bytes through a real decoder, as the link does.
        fn hear_bytes(&mut self, bytes: &[u8], decoder: &mut Decoder) {
            let (machine, now) = (&mut self.machine, self.now);
            decoder.feed(bytes, |event| machine.on_event(event, now));
        }

        fn say(&mut self) -> Vec<Message> {
            std::iter::from_fn(|| self.machine.next_message(self.now)).collect()
        }

        fn greet(&mut self) {
            self.say();
            self.hear(ready(PROTO));
        }

        /// Everything up to and including `PushEnd`, answering the begin with `PushAccepted`.
        fn push_it_all(&mut self) -> Vec<Message> {
            self.greet();
            let mut sent = self.say();
            self.hear(Message::PushAccepted);
            sent.extend(self.say());
            sent
        }
    }

    fn frames(msgs: &[Message]) -> Vec<u8> {
        let mut out = Vec::new();
        for msg in msgs {
            assert!(let Ok(()) = encode(msg, &mut out));
        }
        out
    }

    #[test]
    fn hello_repeats_on_its_interval_until_ready_and_then_stops() {
        let mut rig = Rig::new(Goal::Handshake);
        check!(rig.say() == [Message::Hello { proto: PROTO }]);
        check!(rig.say().is_empty(), "not again before the interval");
        rig.advance(HELLO_EVERY);
        check!(rig.say() == [Message::Hello { proto: PROTO }]);
        rig.advance(HELLO_EVERY);
        check!(rig.say() == [Message::Hello { proto: PROTO }]);

        rig.hear(ready(PROTO));
        rig.advance(HELLO_EVERY * 4);
        check!(rig.say().is_empty());
        assert!(let Some(Ok(Outcome::Ready(info))) = rig.machine.take_result());
        check!(info.build.as_str() == "ferrite-loader test");
    }

    #[test]
    fn ready_that_never_comes_times_out() {
        let mut rig = Rig::new(Goal::Handshake);
        rig.advance(Duration::from_secs(5));
        rig.machine.tick(rig.now);
        assert!(let Some(Err(ConversationError::Timeout { waiting_for: Stage::Greeting, .. })) = rig.machine.take_result());
    }

    #[test]
    fn a_greeting_timeout_can_restart_but_a_rejection_cannot() {
        let mut rig = Rig::new(Goal::Handshake);
        rig.advance(Duration::from_secs(6));
        rig.machine.tick(rig.now);
        check!(rig.machine.restart_greeting(rig.now, Duration::from_secs(120)));
        check!(rig.machine.deadline() == Some(rig.now + Duration::from_secs(120)));
        check!(rig.say() == [Message::Hello { proto: PROTO }], "greets again at once");

        let mut rig = Rig::push(PushKind::App, 8);
        rig.greet();
        rig.hear(Message::PushRejected { reason: "no".into() });
        check!(!rig.machine.restart_greeting(rig.now, Duration::from_secs(1)));
    }

    #[test]
    fn a_protocol_mismatch_ends_the_conversation_before_any_push() {
        let mut rig = Rig::push(PushKind::App, 8);
        rig.say();
        rig.hear(ready(PROTO + 1));
        check!(
            rig.machine.take_result() == Some(Err(ConversationError::ProtoMismatch { ours: PROTO, theirs: PROTO + 1 }))
        );
        check!(rig.say().is_empty(), "no PushBegin after a mismatch");
    }

    #[test]
    fn a_reject_at_begin_stops_before_any_data_goes_out() {
        let mut rig = Rig::push(PushKind::App, 40_000);
        rig.greet();
        check!(matches!(rig.say().as_slice(), [Message::PushBegin(_)]));
        rig.hear(Message::PushRejected { reason: "busy: an app is running".into() });
        check!(
            rig.machine.take_result()
                == Some(Err(ConversationError::Rejected {
                    reason: "busy: an app is running".into(),
                    after_end: false
                }))
        );
        check!(rig.say().is_empty());
    }

    #[test]
    fn data_goes_out_in_push_chunks_from_offset_zero_and_a_reject_after_the_end_is_reported() {
        let len = PUSH_CHUNK * 2 + 100;
        let mut rig = Rig::push(PushKind::App, len);
        let sent = rig.push_it_all();

        assert!(let [Message::PushBegin(begin), Message::PushData { offset: 0, bytes: a }, Message::PushData { offset: o2, bytes: b }, Message::PushData { offset: o3, bytes: c }, Message::PushEnd] = sent.as_slice());
        check!((begin.size, begin.watchdog_secs, begin.kind) == (len as u64, 60, PushKind::App));
        check!((*o2, *o3) == (PUSH_CHUNK as u64, 2 * PUSH_CHUNK as u64));
        check!((a.len(), b.len(), c.len()) == (PUSH_CHUNK, PUSH_CHUNK, 100));
        let image: Vec<u8> = [a.as_slice(), b, c].concat();
        check!(crc32fast::hash(&image) == begin.crc32);

        rig.hear(Message::PushRejected { reason: "image crc32 is 0x1, expected 0x2".into() });
        assert!(let Some(Err(ConversationError::Rejected { after_end: true, .. })) = rig.machine.take_result());
    }

    #[test]
    fn a_full_push_streams_output_and_ends_with_the_apps_status() {
        let mut rig = Rig::push(PushKind::App, 10);
        rig.push_it_all();
        rig.hear(Message::Started { name: short("app.efi") });
        rig.hear(Message::Output { stream: Stream::Log, bytes: b"[ 0.001] hello".to_vec() });
        rig.hear(Message::Output { stream: Stream::Data, bytes: vec![0, 1, 2] });
        rig.hear(Message::Exited(Exited::new(STATUS_ABORTED, Duration::from_millis(1234))));

        assert!(let Some(Ok(Outcome::Exited(exited))) = rig.machine.take_result());
        check!((exited.status, exited.elapsed_ms) == (STATUS_ABORTED, 1234));
        let effects = rig.machine.take_effects();
        check!(effects.contains(&Effect::Started(short("app.efi"))));
        check!(effects.contains(&Effect::Output { stream: Stream::Data, bytes: vec![0, 1, 2] }));
    }

    #[test]
    fn a_running_app_is_bounded_by_its_watchdog_plus_the_grace() {
        let mut rig = Rig::push(PushKind::App, 10);
        rig.push_it_all();
        rig.hear(Message::Started { name: short("app.efi") });
        rig.advance(WATCHDOG_GRACE + Duration::from_secs(59));
        rig.machine.tick(rig.now);
        check!(rig.machine.take_result().is_none());
        rig.advance(Duration::from_secs(1));
        rig.machine.tick(rig.now);
        assert!(let Some(Err(ConversationError::Timeout { waiting_for: Stage::Running, .. })) = rig.machine.take_result());
    }

    #[test]
    fn a_loader_push_needs_started_and_then_a_new_ready() {
        let mut rig = Rig::push(PushKind::Loader, 10);
        rig.push_it_all();
        rig.hear(ready(PROTO));
        check!(rig.machine.take_result().is_none(), "a Ready before Started is not the replacement");

        rig.hear(Message::Started { name: short("app.efi") });
        check!(rig.machine.stage() == Stage::AwaitReplacement);
        check!(rig.say().is_empty(), "nothing is sent to the old loader as it hands over");

        check!(rig.machine.link_lost(rig.now), "the link dropping is expected here");
        check!(rig.say() == [Message::Hello { proto: PROTO }], "greets the new loader once the link is back");
        rig.hear(ready_as(PROTO, NEW));
        assert!(let Some(Ok(Outcome::Replaced(info))) = rig.machine.take_result());
        check!((info.proto, info.identity) == (PROTO, NEW));
    }

    #[test]
    fn only_a_ready_with_a_new_instance_completes_a_loader_push() {
        let mut rig = Rig::push(PushKind::Loader, 10);
        rig.push_it_all();
        rig.hear(Message::Started { name: short("app.efi") });

        rig.hear(ready(PROTO));
        check!(rig.machine.take_result().is_none(), "a late answer from the old loader is ignored");
        check!(rig.machine.stage() == Stage::AwaitReplacement);

        rig.hear(ready_as(PROTO, NEW));
        assert!(let Some(Ok(Outcome::Replaced(info))) = rig.machine.take_result());
        check!(info.identity.instance == NEW.instance);
    }

    #[test]
    fn a_replacement_that_returned_is_reported_not_mistaken_for_success() {
        let mut rig = Rig::push(PushKind::Loader, 10);
        rig.push_it_all();
        rig.hear(Message::Started { name: short("app.efi") });
        let resumed = Identity { boot: OLD.boot + 1, ..OLD };
        rig.hear(ready_as(PROTO, resumed));
        check!(rig.machine.take_result() == Some(Err(ConversationError::ReplacementReturned { identity: resumed })));
    }

    #[test]
    fn a_replacement_of_another_protocol_is_refused() {
        let mut rig = Rig::push(PushKind::Loader, 10);
        rig.push_it_all();
        rig.hear(Message::Started { name: short("app.efi") });
        rig.hear(ready_as(PROTO + 1, NEW));
        check!(
            rig.machine.take_result() == Some(Err(ConversationError::ProtoMismatch { ours: PROTO, theirs: PROTO + 1 }))
        );
    }

    #[test]
    fn a_loader_push_whose_started_was_lost_with_the_link_still_counts_as_handed_over() {
        let mut rig = Rig::push(PushKind::Loader, 10);
        rig.push_it_all();
        check!(rig.machine.link_lost(rig.now));
        check!(rig.machine.stage() == Stage::AwaitReplacement);
        rig.hear(ready_as(PROTO, NEW));
        assert!(let Some(Ok(Outcome::Replaced(_))) = rig.machine.take_result());
    }

    #[test]
    fn losing_the_link_mid_push_or_mid_run_is_fatal_but_not_before_ready() {
        let mut rig = Rig::push(PushKind::App, 10);
        check!(rig.machine.link_lost(rig.now));
        rig.greet();
        rig.say();
        check!(!rig.machine.link_lost(rig.now));
        check!(rig.machine.take_result() == Some(Err(ConversationError::LinkLost { during: Stage::AwaitAccept })));

        let mut rig = Rig::push(PushKind::App, 10);
        rig.push_it_all();
        rig.hear(Message::Started { name: short("app.efi") });
        check!(!rig.machine.link_lost(rig.now));
        check!(rig.machine.take_result() == Some(Err(ConversationError::LinkLost { during: Stage::Running })));
    }

    #[test]
    fn a_fault_is_only_shown_and_a_rejection_after_the_end_or_the_handover_is_what_ends_a_push() {
        for stage_reached in [Stage::AwaitStart, Stage::Running, Stage::AwaitReplacement] {
            let kind = if stage_reached == Stage::AwaitReplacement { PushKind::Loader } else { PushKind::App };
            let mut rig = Rig::push(kind, 10);
            rig.push_it_all();
            if stage_reached != Stage::AwaitStart {
                rig.hear(Message::Started { name: short("app.efi") });
            }
            check!(rig.machine.stage() == stage_reached);
            rig.hear(Message::Fault { text: "malformed Input".into() });
            check!(rig.machine.take_result().is_none(), "{stage_reached:?}");
            check!(rig.machine.take_effects().contains(&Effect::Fault("malformed Input".into())));
        }

        let mut rig = Rig::push(PushKind::Loader, 10);
        rig.push_it_all();
        rig.hear(Message::Started { name: short("app.efi") });
        rig.hear(Message::PushRejected { reason: "app.efi not started: the link would not uninstall".into() });
        assert!(let Some(Err(ConversationError::Rejected { after_end: true, .. })) = rig.machine.take_result());
    }

    fn running() -> Rig {
        let mut rig = Rig::push(PushKind::App, 10);
        rig.push_it_all();
        rig.hear(Message::Started { name: short("app.efi") });
        check!(rig.machine.stage() == Stage::Running);
        rig
    }

    #[test]
    fn stop_after_sends_stop_once_when_the_app_has_run_that_long() {
        let mut rig = running();
        rig.machine = rig.machine.with_stop_after(Duration::from_secs(90));
        rig.advance(Duration::from_secs(89));
        check!(rig.say().is_empty());
        rig.advance(Duration::from_secs(1));
        check!(rig.say() == [Message::Stop]);
        rig.advance(Duration::from_secs(60));
        check!(rig.say().is_empty(), "Stop goes out once");

        rig.hear(Message::Exited(Exited::new(0, Duration::from_secs(91))));
        assert!(let Some(Ok(Outcome::Exited(exited))) = rig.machine.take_result());
        check!(exited.status == 0);
    }

    #[test]
    fn stop_after_counts_from_started_not_from_the_start_of_the_push() {
        let mut rig = Rig::push(PushKind::App, 10);
        rig.machine = rig.machine.with_stop_after(Duration::from_secs(10));
        rig.push_it_all();
        rig.advance(Duration::from_secs(12));
        check!(rig.say().is_empty(), "the app has not started");
        rig.hear(Message::Started { name: short("app.efi") });
        rig.advance(Duration::from_secs(9));
        check!(rig.say().is_empty());
        rig.advance(Duration::from_secs(1));
        check!(rig.say() == [Message::Stop]);
    }

    #[test]
    fn the_first_interrupt_asks_the_app_to_stop_and_the_second_leaves_it_running() {
        let mut rig = running();
        check!(rig.machine.interrupt() == Interrupted::Asked);
        check!(rig.say() == [Message::Stop]);
        check!(rig.say().is_empty());
        check!(!rig.machine.is_finished(), "still waiting for the app's own exit");

        check!(rig.machine.interrupt() == Interrupted::Leave);
        let notes: Vec<_> = rig
            .machine
            .take_effects()
            .into_iter()
            .filter_map(|e| if let Effect::Note(text) = e { Some(text) } else { None })
            .collect();
        check!(notes.iter().any(|n| n.contains("leaving the app running")), "{notes:?}");
        check!(rig.say().is_empty(), "no second Stop");
    }

    #[test]
    fn an_interrupt_after_an_asked_stop_does_not_repeat_it_when_stop_after_comes_due() {
        let mut rig = running();
        rig.machine = rig.machine.with_stop_after(Duration::from_secs(5));
        check!(rig.machine.interrupt() == Interrupted::Asked);
        rig.advance(Duration::from_secs(6));
        check!(rig.say() == [Message::Stop]);
    }

    #[test]
    fn an_interrupt_with_no_app_running_just_ends_the_run() {
        let mut rig = Rig::push(PushKind::App, 10);
        check!(rig.machine.interrupt() == Interrupted::Exit, "greeting");
        rig.greet();
        check!(rig.machine.interrupt() == Interrupted::Exit, "waiting for the push to be accepted");

        let mut rig = Rig::push(PushKind::Loader, 10);
        rig.push_it_all();
        rig.hear(Message::Started { name: short("app.efi") });
        check!(rig.machine.interrupt() == Interrupted::Exit, "waiting for a replacement loader");
    }

    #[test]
    fn a_running_app_that_keeps_talking_is_not_timed_out_at_its_watchdog() {
        let mut rig = running();
        rig.advance(WATCHDOG_GRACE + Duration::from_secs(50));
        rig.hear(Message::Output { stream: Stream::Log, bytes: b"alive".to_vec() });
        rig.advance(WATCHDOG_GRACE + Duration::from_secs(59));
        rig.machine.tick(rig.now);
        check!(rig.machine.take_result().is_none(), "the deadline moved with the last word from the loader");
        rig.advance(Duration::from_secs(1));
        rig.machine.tick(rig.now);
        assert!(let Some(Err(ConversationError::Timeout { waiting_for: Stage::Running, .. })) = rig.machine.take_result());
    }

    #[test]
    fn garbage_between_frames_is_skipped_and_the_run_still_completes() {
        let mut rig = Rig::push(PushKind::App, 10);
        rig.push_it_all();
        let mut decoder = Decoder::new();
        let mut bytes = frames(&[Message::Started { name: short("app.efi") }]);
        bytes.extend_from_slice(&[0x00, 0xFE, 0x13, 0x37, 0xFE]);
        bytes.extend(frames(&[
            Message::Output { stream: Stream::Log, bytes: b"line".to_vec() },
            Message::Exited(Exited::new(0, Duration::from_millis(5))),
        ]));
        for chunk in bytes.chunks(3) {
            rig.hear_bytes(chunk, &mut decoder);
        }
        assert!(let Some(Ok(Outcome::Exited(_))) = rig.machine.take_result());
        let effects = rig.machine.take_effects();
        check!(effects.iter().any(|e| matches!(e, Effect::Dropped(n) if *n > 0)), "{effects:?}");
        check!(effects.contains(&Effect::Output { stream: Stream::Log, bytes: b"line".to_vec() }));
    }

    #[test]
    fn a_corrupted_output_frame_is_lost_alone() {
        let mut rig = Rig::push(PushKind::App, 10);
        rig.push_it_all();
        rig.hear(Message::Started { name: short("app.efi") });
        let mut damaged = frames(&[Message::Output { stream: Stream::Log, bytes: b"lost".to_vec() }]);
        let last = damaged.len() - 1;
        damaged[last] ^= 0xFF;
        let mut bytes = damaged;
        bytes.extend(frames(&[
            Message::Output { stream: Stream::Log, bytes: b"kept".to_vec() },
            Message::Exited(Exited::new(0, Duration::ZERO)),
        ]));
        rig.hear_bytes(&bytes, &mut Decoder::new());
        assert!(let Some(Ok(Outcome::Exited(_))) = rig.machine.take_result());
        let outputs: Vec<_> = rig
            .machine
            .take_effects()
            .into_iter()
            .filter_map(|e| if let Effect::Output { bytes, .. } = e { Some(bytes) } else { None })
            .collect();
        check!(outputs == [b"kept".to_vec()]);
    }

    #[test]
    fn a_lost_push_accepted_ends_in_a_timeout_not_a_hang() {
        let mut rig = Rig::push(PushKind::App, 10);
        rig.greet();
        rig.say();
        let mut decoder = Decoder::new();
        let mut accepted = frames(&[Message::PushAccepted]);
        accepted[4] ^= 0x01;
        rig.hear_bytes(&accepted, &mut decoder);
        check!(rig.say().is_empty(), "no data goes out before the accept");
        rig.advance(REPLY_WITHIN);
        rig.machine.tick(rig.now);
        assert!(let Some(Err(ConversationError::Timeout { waiting_for: Stage::AwaitAccept, .. })) = rig.machine.take_result());
    }
}
