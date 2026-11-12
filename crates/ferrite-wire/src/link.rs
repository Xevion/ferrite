//! The loader link protocol: framed messages between the desktop and the resident loader.
//!
//! The loader keeps the DbC serial link up and the desktop pushes images over it. Both ends use
//! this module: [`encode`] turns a [`Message`] into a frame and a [`Decoder`] turns a byte stream
//! cut at arbitrary points back into [`Event`]s.
//!
//! # Frame
//!
//! All integers are little-endian.
//!
//! | Field | Size | Meaning |
//! | --- | --- | --- |
//! | sync | 2 | [`SYNC`], `0xFE 0x4C` |
//! | type | 1 | [`MessageType`] code |
//! | length | 4 | payload length, at most [`MAX_PAYLOAD`] (64 KiB) |
//! | payload | length | per message, below |
//! | crc | 4 | CRC-32 (IEEE) of type, length and payload |
//!
//! A decoder that sees a bad length or CRC drops one byte and scans for the next sync, so it
//! recovers from garbage and from a corrupted frame without losing the frames behind it. The
//! dropped bytes are reported as [`Event::Dropped`]. A message is never split across frames:
//! image data is chunked by the sender ([`PUSH_CHUNK`] is a good size).
//!
//! # Messages
//!
//! Strings marked short are a `u8` length followed by that many UTF-8 bytes, at most
//! [`MAX_SHORT`]; the others take the rest of the payload.
//!
//! Desktop to loader:
//!
//! | Code | Message | Payload |
//! | --- | --- | --- |
//! | `0x01` | [`Message::Hello`] | `proto: u16` |
//! | `0x02` | [`Message::PushBegin`] | `kind: u8` (0 app, 1 loader), `size: u64`, `crc32: u32`, `watchdog_secs: u32`, `name` (short) |
//! | `0x03` | [`Message::PushData`] | `offset: u64`, then the bytes |
//! | `0x04` | [`Message::PushEnd`] | empty |
//! | `0x05` | [`Message::Input`] | the bytes |
//! | `0x06` | [`Message::Abort`] | empty |
//! | `0x07` | [`Message::Reboot`] | empty |
//!
//! Loader to desktop:
//!
//! | Code | Message | Payload |
//! | --- | --- | --- |
//! | `0x81` | [`Message::Ready`] | `proto: u16`, `build` (short) |
//! | `0x82` | [`Message::PushAccepted`] | empty |
//! | `0x83` | [`Message::PushRejected`] | `reason`, UTF-8 |
//! | `0x84` | [`Message::Started`] | `name` (short) |
//! | `0x85` | [`Message::Output`] | `stream: u8` (0 log, 1 data), then the bytes |
//! | `0x86` | [`Message::Exited`] | `status: u64`, `elapsed_ms: u32` |
//! | `0x87` | [`Message::Fault`] | `text`, UTF-8 |
//!
//! # Conversation
//!
//! - Bytes sent while the desktop's port is closed are lost, so the desktop sends `Hello` after it
//!   opens the link and again until it sees `Ready`. The loader also sends `Ready` unprompted each
//!   time the link is (re)configured.
//! - A push is `PushBegin`, then `PushData` chunks with contiguous offsets from 0, then `PushEnd`.
//!   The loader answers `PushBegin` with `PushAccepted` or `PushRejected`. A chunk at the wrong
//!   offset, an image that does not match `size` and `crc32`, or an image the firmware refuses to
//!   load, ends the push with `PushRejected`; the desktop starts over with a new `PushBegin`.
//!   `Abort` drops a push in progress and has no effect on a running app.
//! - An accepted `App` push runs: `Started`, any number of `Output`, then `Exited`. The loader arms
//!   the firmware watchdog for `watchdog_secs` first; `0` is refused. When it expires the machine
//!   resets and the link drops.
//! - While an app runs, `Input` reaches the app's read side and `Reboot` resets the machine; a
//!   `PushBegin` is refused as busy.
//! - A `Loader` push replaces the running loader: after `Started` the link drops while the new
//!   loader brings it up, and the new loader's `Ready` is the next message.
//! - `Fault` reports something the loader could not do (a malformed message, a watchdog that would
//!   not arm); the text is for a human.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use core::ops::Deref;
use core::time::Duration;

use snafu::{Snafu, ensure};

/// Version carried by [`Message::Hello`] and [`Message::Ready`].
pub const PROTO: u16 = 1;

/// First bytes of every frame.
pub const SYNC: [u8; 2] = [0xFE, 0x4C];
/// Bytes before the payload: sync, type and length.
pub const HEADER_LEN: usize = 7;
pub const CRC_LEN: usize = 4;
/// Longest payload a frame may carry.
pub const MAX_PAYLOAD: usize = 64 * 1024;
/// Longest whole frame.
pub const MAX_FRAME: usize = HEADER_LEN + MAX_PAYLOAD + CRC_LEN;
/// Longest short string.
pub const MAX_SHORT: usize = 64;
/// Most bytes one [`Message::PushData`] can carry.
pub const MAX_PUSH_DATA: usize = MAX_PAYLOAD - 8;
/// Most bytes one [`Message::Output`] can carry.
pub const MAX_OUTPUT: usize = MAX_PAYLOAD - 1;
/// Image bytes per [`Message::PushData`] that the desktop should send.
pub const PUSH_CHUNK: usize = 16 * 1024;
/// Watchdog budget a push should carry unless the user asks for another.
pub const DEFAULT_WATCHDOG_SECS: u32 = 300;

/// A string of at most [`MAX_SHORT`] bytes, checked when it is made.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct ShortStr(String);

impl ShortStr {
    /// Refuses a string longer than [`MAX_SHORT`] bytes rather than cutting it.
    pub fn new(text: &str) -> Result<Self, ShortStrError> {
        ensure!(text.len() <= MAX_SHORT, ShortStrSnafu { len: text.len() });
        Ok(Self(String::from(text)))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Deref for ShortStr {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ShortStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<&str> for ShortStr {
    type Error = ShortStrError;

    fn try_from(text: &str) -> Result<Self, ShortStrError> {
        Self::new(text)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Snafu)]
#[snafu(display("a short string is at most {MAX_SHORT} bytes, got {len}"))]
pub struct ShortStrError {
    pub len: usize,
}

/// The type byte of a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageType {
    Hello,
    PushBegin,
    PushData,
    PushEnd,
    Input,
    Abort,
    Reboot,
    Ready,
    PushAccepted,
    PushRejected,
    Started,
    Output,
    Exited,
    Fault,
    Unknown(u8),
}

impl MessageType {
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Hello => 0x01,
            Self::PushBegin => 0x02,
            Self::PushData => 0x03,
            Self::PushEnd => 0x04,
            Self::Input => 0x05,
            Self::Abort => 0x06,
            Self::Reboot => 0x07,
            Self::Ready => 0x81,
            Self::PushAccepted => 0x82,
            Self::PushRejected => 0x83,
            Self::Started => 0x84,
            Self::Output => 0x85,
            Self::Exited => 0x86,
            Self::Fault => 0x87,
            Self::Unknown(raw) => raw,
        }
    }
}

impl From<u8> for MessageType {
    fn from(raw: u8) -> Self {
        match raw {
            0x01 => Self::Hello,
            0x02 => Self::PushBegin,
            0x03 => Self::PushData,
            0x04 => Self::PushEnd,
            0x05 => Self::Input,
            0x06 => Self::Abort,
            0x07 => Self::Reboot,
            0x81 => Self::Ready,
            0x82 => Self::PushAccepted,
            0x83 => Self::PushRejected,
            0x84 => Self::Started,
            0x85 => Self::Output,
            0x86 => Self::Exited,
            0x87 => Self::Fault,
            other => Self::Unknown(other),
        }
    }
}

/// What a pushed image is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushKind {
    /// Run once, report its exit status and wait for the next push.
    App,
    /// Replace the running loader.
    Loader,
    Unknown(u8),
}

impl PushKind {
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::App => 0,
            Self::Loader => 1,
            Self::Unknown(raw) => raw,
        }
    }
}

impl From<u8> for PushKind {
    fn from(raw: u8) -> Self {
        match raw {
            0 => Self::App,
            1 => Self::Loader,
            other => Self::Unknown(other),
        }
    }
}

/// Which stream an [`Message::Output`] belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    /// Text the app logged, one record per message.
    Log,
    /// Whatever the app chooses to send.
    Data,
    Unknown(u8),
}

impl Stream {
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Log => 0,
            Self::Data => 1,
            Self::Unknown(raw) => raw,
        }
    }
}

impl From<u8> for Stream {
    fn from(raw: u8) -> Self {
        match raw {
            0 => Self::Log,
            1 => Self::Data,
            other => Self::Unknown(other),
        }
    }
}

/// The announcement of an image about to be pushed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushBegin {
    pub kind: PushKind,
    /// Image length in bytes.
    pub size: u64,
    /// CRC-32 (IEEE) of the whole image.
    pub crc32: u32,
    /// Watchdog budget for the run; `0` is refused.
    pub watchdog_secs: u32,
    pub name: ShortStr,
}

impl PushBegin {
    #[must_use]
    pub const fn watchdog(&self) -> Duration {
        Duration::from_secs(self.watchdog_secs as u64)
    }
}

/// How an app run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Exited {
    /// The UEFI status the image returned.
    pub status: u64,
    pub elapsed_ms: u32,
}

impl Exited {
    /// `elapsed` rounded down to milliseconds, saturating at `u32::MAX`.
    #[must_use]
    pub fn new(status: u64, elapsed: Duration) -> Self {
        Self { status, elapsed_ms: u32::try_from(elapsed.as_millis()).unwrap_or(u32::MAX) }
    }

    #[must_use]
    pub const fn elapsed(&self) -> Duration {
        Duration::from_millis(self.elapsed_ms as u64)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    Hello {
        proto: u16,
    },
    PushBegin(PushBegin),
    PushData {
        offset: u64,
        bytes: Vec<u8>,
    },
    PushEnd,
    /// Bytes for the running app's read side.
    Input {
        bytes: Vec<u8>,
    },
    Abort,
    Reboot,
    Ready {
        proto: u16,
        build: ShortStr,
    },
    PushAccepted,
    PushRejected {
        reason: String,
    },
    Started {
        name: ShortStr,
    },
    Output {
        stream: Stream,
        bytes: Vec<u8>,
    },
    Exited(Exited),
    Fault {
        text: String,
    },
    /// A well-formed frame of a type this version does not know. Never encoded.
    Unknown {
        code: u8,
        len: usize,
    },
}

impl Message {
    #[must_use]
    pub const fn message_type(&self) -> MessageType {
        match self {
            Self::Hello { .. } => MessageType::Hello,
            Self::PushBegin(_) => MessageType::PushBegin,
            Self::PushData { .. } => MessageType::PushData,
            Self::PushEnd => MessageType::PushEnd,
            Self::Input { .. } => MessageType::Input,
            Self::Abort => MessageType::Abort,
            Self::Reboot => MessageType::Reboot,
            Self::Ready { .. } => MessageType::Ready,
            Self::PushAccepted => MessageType::PushAccepted,
            Self::PushRejected { .. } => MessageType::PushRejected,
            Self::Started { .. } => MessageType::Started,
            Self::Output { .. } => MessageType::Output,
            Self::Exited(_) => MessageType::Exited,
            Self::Fault { .. } => MessageType::Fault,
            Self::Unknown { code, .. } => MessageType::Unknown(*code),
        }
    }

    /// Payload length, or `None` for [`Message::Unknown`], which has no encoding.
    fn payload_len(&self) -> Option<usize> {
        Some(match self {
            Self::Hello { .. } => 2,
            Self::PushBegin(begin) => 1 + 8 + 4 + 4 + 1 + begin.name.len(),
            Self::PushData { bytes, .. } => 8 + bytes.len(),
            Self::PushEnd | Self::Abort | Self::Reboot | Self::PushAccepted => 0,
            Self::Input { bytes } => bytes.len(),
            Self::Ready { build, .. } => 2 + 1 + build.len(),
            Self::PushRejected { reason } => reason.len(),
            Self::Started { name } => 1 + name.len(),
            Self::Output { bytes, .. } => 1 + bytes.len(),
            Self::Exited(_) => 12,
            Self::Fault { text } => text.len(),
            Self::Unknown { .. } => return None,
        })
    }

    fn write_payload(&self, out: &mut Vec<u8>) {
        fn short(out: &mut Vec<u8>, text: &ShortStr) {
            // A ShortStr is at most MAX_SHORT bytes, so the length fits.
            out.push(text.len() as u8);
            out.extend_from_slice(text.as_bytes());
        }
        match self {
            Self::Hello { proto } => out.extend_from_slice(&proto.to_le_bytes()),
            Self::PushBegin(begin) => {
                out.push(begin.kind.code());
                out.extend_from_slice(&begin.size.to_le_bytes());
                out.extend_from_slice(&begin.crc32.to_le_bytes());
                out.extend_from_slice(&begin.watchdog_secs.to_le_bytes());
                short(out, &begin.name);
            },
            Self::PushData { offset, bytes } => {
                out.extend_from_slice(&offset.to_le_bytes());
                out.extend_from_slice(bytes);
            },
            Self::PushEnd | Self::Abort | Self::Reboot | Self::PushAccepted | Self::Unknown { .. } => {},
            Self::Input { bytes } => out.extend_from_slice(bytes),
            Self::Ready { proto, build } => {
                out.extend_from_slice(&proto.to_le_bytes());
                short(out, build);
            },
            Self::PushRejected { reason } => out.extend_from_slice(reason.as_bytes()),
            Self::Started { name } => short(out, name),
            Self::Output { stream, bytes } => {
                out.push(stream.code());
                out.extend_from_slice(bytes);
            },
            Self::Exited(exited) => {
                out.extend_from_slice(&exited.status.to_le_bytes());
                out.extend_from_slice(&exited.elapsed_ms.to_le_bytes());
            },
            Self::Fault { text } => out.extend_from_slice(text.as_bytes()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Snafu)]
pub enum EncodeError {
    #[snafu(display("a payload of {len} bytes exceeds the {MAX_PAYLOAD} byte limit"))]
    TooLarge { len: usize },
    #[snafu(display("a message of unknown type {code:#04x} has no encoding"))]
    Unencodable { code: u8 },
}

/// Append the frame for `msg` to `out`. On error `out` is left as it was.
pub fn encode(msg: &Message, out: &mut Vec<u8>) -> Result<(), EncodeError> {
    let len = msg.payload_len().ok_or_else(|| EncodeError::Unencodable { code: msg.message_type().code() })?;
    frame(msg.message_type(), len, out, |out| msg.write_payload(out))
}

/// Append an [`Message::Output`] frame without first copying `bytes` into a message.
pub fn encode_output(stream: Stream, bytes: &[u8], out: &mut Vec<u8>) -> Result<(), EncodeError> {
    frame(MessageType::Output, 1 + bytes.len(), out, |out| {
        out.push(stream.code());
        out.extend_from_slice(bytes);
    })
}

/// Bytes [`encode_output`] appends for `len` bytes of output.
#[must_use]
pub const fn output_frame_len(len: usize) -> usize {
    HEADER_LEN + 1 + len + CRC_LEN
}

fn frame(
    ty: MessageType,
    len: usize,
    out: &mut Vec<u8>,
    write_payload: impl FnOnce(&mut Vec<u8>),
) -> Result<(), EncodeError> {
    ensure!(len <= MAX_PAYLOAD, TooLargeSnafu { len });
    let start = out.len();
    out.reserve(HEADER_LEN + len + CRC_LEN);
    out.extend_from_slice(&SYNC);
    out.push(ty.code());
    // The length was checked against MAX_PAYLOAD above.
    out.extend_from_slice(&(len as u32).to_le_bytes());
    write_payload(out);
    let crc = crc32fast::hash(&out[start + SYNC.len()..]);
    out.extend_from_slice(&crc.to_le_bytes());
    Ok(())
}

/// Why a well-framed payload could not be read as its message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Snafu)]
pub enum PayloadError {
    #[snafu(display("{len} bytes is not a valid payload length for this message"))]
    Length { len: usize },
    #[snafu(display("text is not valid UTF-8"))]
    NotUtf8,
    #[snafu(display("a short string of {len} bytes exceeds the {MAX_SHORT} byte limit"))]
    ShortTooLong { len: usize },
}

/// A cursor over a payload.
struct Reader<'a> {
    rest: &'a [u8],
    total: usize,
}

impl<'a> Reader<'a> {
    const fn new(payload: &'a [u8]) -> Self {
        Self { rest: payload, total: payload.len() }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], PayloadError> {
        let (head, tail) = self.rest.split_at_checked(n).ok_or(PayloadError::Length { len: self.total })?;
        self.rest = tail;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], PayloadError> {
        let mut out = [0; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, PayloadError> {
        Ok(self.array::<1>()?[0])
    }

    fn u16(&mut self) -> Result<u16, PayloadError> {
        self.array().map(u16::from_le_bytes)
    }

    fn u32(&mut self) -> Result<u32, PayloadError> {
        self.array().map(u32::from_le_bytes)
    }

    fn u64(&mut self) -> Result<u64, PayloadError> {
        self.array().map(u64::from_le_bytes)
    }

    fn short(&mut self) -> Result<ShortStr, PayloadError> {
        let len = usize::from(self.u8()?);
        let text = core::str::from_utf8(self.take(len)?).map_err(|_| PayloadError::NotUtf8)?;
        ShortStr::new(text).map_err(|e| PayloadError::ShortTooLong { len: e.len })
    }

    fn rest(&mut self) -> &'a [u8] {
        core::mem::take(&mut self.rest)
    }

    fn rest_text(&mut self) -> Result<String, PayloadError> {
        core::str::from_utf8(self.rest()).map(String::from).map_err(|_| PayloadError::NotUtf8)
    }

    fn finish(self) -> Result<(), PayloadError> {
        ensure!(self.rest.is_empty(), LengthSnafu { len: self.total });
        Ok(())
    }
}

fn decode_payload(ty: MessageType, payload: &[u8]) -> Result<Message, PayloadError> {
    let mut r = Reader::new(payload);
    let msg = match ty {
        MessageType::Hello => Message::Hello { proto: r.u16()? },
        MessageType::PushBegin => Message::PushBegin(PushBegin {
            kind: PushKind::from(r.u8()?),
            size: r.u64()?,
            crc32: r.u32()?,
            watchdog_secs: r.u32()?,
            name: r.short()?,
        }),
        MessageType::PushData => Message::PushData { offset: r.u64()?, bytes: r.rest().to_vec() },
        MessageType::PushEnd => Message::PushEnd,
        MessageType::Input => Message::Input { bytes: r.rest().to_vec() },
        MessageType::Abort => Message::Abort,
        MessageType::Reboot => Message::Reboot,
        MessageType::Ready => Message::Ready { proto: r.u16()?, build: r.short()? },
        MessageType::PushAccepted => Message::PushAccepted,
        MessageType::PushRejected => Message::PushRejected { reason: r.rest_text()? },
        MessageType::Started => Message::Started { name: r.short()? },
        MessageType::Output => Message::Output { stream: Stream::from(r.u8()?), bytes: r.rest().to_vec() },
        MessageType::Exited => Message::Exited(Exited { status: r.u64()?, elapsed_ms: r.u32()? }),
        MessageType::Fault => Message::Fault { text: r.rest_text()? },
        MessageType::Unknown(code) => return Ok(Message::Unknown { code, len: payload.len() }),
    };
    r.finish()?;
    Ok(msg)
}

/// What a [`Decoder`] found in the stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Message(Message),
    /// Bytes skipped while looking for the next frame.
    Dropped {
        bytes: usize,
    },
    /// A frame with a good CRC whose payload did not fit its type. The frame is consumed.
    Malformed {
        ty: MessageType,
        error: PayloadError,
    },
}

/// Turns a byte stream into events, resynchronising on [`SYNC`] after garbage or a bad frame.
///
/// A frame is checked once it is complete, so a corrupted length can hold back the frames behind it
/// until up to [`MAX_FRAME`] bytes have arrived; none are lost.
#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
    pos: usize,
    dropped: usize,
}

impl Decoder {
    #[must_use]
    pub const fn new() -> Self {
        Self { buf: Vec::new(), pos: 0, dropped: 0 }
    }

    /// Bytes held for a frame that is not complete yet.
    #[must_use]
    pub const fn buffered(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Add `bytes` and report everything they complete, in stream order.
    pub fn feed(&mut self, bytes: &[u8], mut on: impl FnMut(Event)) {
        self.buf.extend_from_slice(bytes);
        while self.step(&mut on) {}
        self.flush_dropped(&mut on);
        self.buf.drain(..self.pos);
        self.pos = 0;
    }

    fn skip(&mut self, n: usize) {
        self.pos += n;
        self.dropped += n;
    }

    fn flush_dropped(&mut self, on: &mut impl FnMut(Event)) {
        if self.dropped > 0 {
            on(Event::Dropped { bytes: core::mem::take(&mut self.dropped) });
        }
    }

    /// Advance past one frame or one stretch of garbage; false when more input is needed.
    fn step(&mut self, on: &mut impl FnMut(Event)) -> bool {
        let avail = &self.buf[self.pos..];
        let Some(at) = avail.windows(SYNC.len()).position(|w| w == SYNC) else {
            // A trailing first sync byte may be the start of the next frame.
            let keep = usize::from(avail.last() == Some(&SYNC[0]));
            let garbage = avail.len() - keep;
            self.skip(garbage);
            return false;
        };
        if at > 0 {
            self.skip(at);
            return true;
        }
        if avail.len() < HEADER_LEN {
            return false;
        }
        let ty = MessageType::from(avail[2]);
        let len = u32::from_le_bytes([avail[3], avail[4], avail[5], avail[6]]) as usize;
        if len > MAX_PAYLOAD {
            self.skip(1);
            return true;
        }
        let total = HEADER_LEN + len + CRC_LEN;
        if avail.len() < total {
            return false;
        }
        let (body, crc) = avail[SYNC.len()..total].split_at(1 + 4 + len);
        let sent = u32::from_le_bytes([crc[0], crc[1], crc[2], crc[3]]);
        if crc32fast::hash(body) != sent {
            self.skip(1);
            return true;
        }
        let event = match decode_payload(ty, &avail[HEADER_LEN..HEADER_LEN + len]) {
            Ok(msg) => Event::Message(msg),
            Err(error) => Event::Malformed { ty, error },
        };
        self.pos += total;
        self.flush_dropped(on);
        on(event);
        true
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;
    use alloc::vec;

    use assert2::{assert, check};
    use rstest::rstest;

    use super::*;

    fn short(text: &str) -> ShortStr {
        ShortStr::new(text).expect("short enough")
    }

    fn hex(bytes: &[u8]) -> String {
        use core::fmt::Write as _;
        bytes.iter().fold(String::new(), |mut hex, b| {
            write!(hex, "{b:02x}").expect("writing to a String");
            hex
        })
    }

    fn bytes_of(hex: &str) -> Vec<u8> {
        (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex digits")).collect()
    }

    fn encoded(msg: &Message) -> Vec<u8> {
        let mut out = Vec::new();
        assert!(let Ok(()) = encode(msg, &mut out));
        out
    }

    fn decode_all(bytes: &[u8]) -> Vec<Event> {
        let mut events = Vec::new();
        Decoder::new().feed(bytes, |e| events.push(e));
        events
    }

    fn begin() -> PushBegin {
        PushBegin {
            kind: PushKind::App,
            size: 0x11_2233_4455,
            crc32: 0xDEAD_BEEF,
            watchdog_secs: 300,
            name: short("hello.efi"),
        }
    }

    /// Pinned bytes: a change here breaks the deployed other end of the link.
    fn golden() -> Vec<(&'static str, Message, &'static str)> {
        vec![
            ("hello", Message::Hello { proto: 1 }, "fe4c010200000001008044c86f"),
            (
                "push_begin",
                Message::PushBegin(begin()),
                "fe4c021b000000005544332211000000efbeadde2c0100000968656c6c6f2e6566695e3f804a",
            ),
            (
                "push_begin_loader",
                Message::PushBegin(PushBegin {
                    kind: PushKind::Loader,
                    size: 1_048_576,
                    crc32: 0x0102_0304,
                    watchdog_secs: 60,
                    name: short("ferrite-loader"),
                }),
                "fe4c0220000000010000100000000000040302013c0000000e666572726974652d6c6f61646572172feba1",
            ),
            (
                "push_data",
                Message::PushData { offset: 0x1_0000, bytes: vec![1, 2, 3, 4] },
                "fe4c030c0000000000010000000000010203041e36ed45",
            ),
            ("push_end", Message::PushEnd, "fe4c0400000000dd51a233"),
            ("input", Message::Input { bytes: b"ls\n".to_vec() }, "fe4c05030000006c730aff3a20b9"),
            ("abort", Message::Abort, "fe4c0600000000bd026249"),
            ("reboot", Message::Reboot, "fe4c07000000000d2b0274"),
            (
                "ready",
                Message::Ready { proto: 1, build: short("ferrite-loader 0.1.0") },
                "fe4c8117000000010014666572726974652d6c6f6164657220302e312e309a78e2df",
            ),
            ("push_accepted", Message::PushAccepted, "fe4c8200000000ef14040d"),
            (
                "push_rejected",
                Message::PushRejected { reason: "watchdog_secs must not be 0".to_string() },
                "fe4c831b0000007761746368646f675f73656373206d757374206e6f742062652030dc38f347",
            ),
            ("started", Message::Started { name: short("hello.efi") }, "fe4c840a0000000968656c6c6f2e6566693a27c063"),
            (
                "output_log",
                Message::Output { stream: Stream::Log, bytes: b"hi\n".to_vec() },
                "fe4c85040000000068690a3cfcaf5c",
            ),
            (
                "output_data_containing_sync",
                Message::Output { stream: Stream::Data, bytes: vec![0xFE, 0x4C, 0, 255] },
                "fe4c850500000001fe4c00ff713c6aa6",
            ),
            (
                "exited",
                Message::Exited(Exited { status: 0, elapsed_ms: 1234 }),
                "fe4c860c0000000000000000000000d20400005b8721f8",
            ),
            (
                "exited_with_error_status",
                Message::Exited(Exited { status: 0x8000_0000_0000_0009, elapsed_ms: u32::MAX }),
                "fe4c860c0000000900000000000080ffffffff75f48181",
            ),
            ("fault", Message::Fault { text: "oops".to_string() }, "fe4c87040000006f6f7073ea458aa6"),
        ]
    }

    #[test]
    fn every_message_encodes_to_its_pinned_bytes_and_decodes_back() {
        for (name, msg, want) in golden() {
            check!(hex(&encoded(&msg)) == want, "{name}");
            check!(decode_all(&bytes_of(want)) == vec![Event::Message(msg)], "{name}");
        }
    }

    #[test]
    fn a_message_decodes_when_fed_split_at_every_byte_boundary() {
        let frame = encoded(&Message::PushData { offset: 7, bytes: vec![0xFE, 0x4C, 9, 9, 9] });
        for cut in 0..=frame.len() {
            let mut decoder = Decoder::new();
            let mut events = Vec::new();
            decoder.feed(&frame[..cut], |e| events.push(e));
            decoder.feed(&frame[cut..], |e| events.push(e));
            check!(
                events == vec![Event::Message(Message::PushData { offset: 7, bytes: vec![0xFE, 0x4C, 9, 9, 9] })],
                "cut at {cut}"
            );
            check!(decoder.buffered() == 0);
        }
        let mut decoder = Decoder::new();
        let mut events = Vec::new();
        for byte in &frame {
            decoder.feed(core::slice::from_ref(byte), |e| events.push(e));
        }
        check!(events.len() == 1);
    }

    #[test]
    fn garbage_before_and_between_frames_is_counted_and_skipped() {
        let mut stream = vec![0x00, 0xFE, 0x01, 0xFE];
        stream.extend(encoded(&Message::Abort));
        stream.extend_from_slice(&[0x55; 5]);
        stream.extend(encoded(&Message::Reboot));
        check!(
            decode_all(&stream)
                == vec![
                    Event::Dropped { bytes: 4 },
                    Event::Message(Message::Abort),
                    Event::Dropped { bytes: 5 },
                    Event::Message(Message::Reboot),
                ]
        );
    }

    #[test]
    fn a_corrupted_crc_drops_that_frame_and_keeps_the_next() {
        let mut first = encoded(&Message::Fault { text: "first".to_string() });
        let flipped = first.len() - 1;
        first[flipped] ^= 0x01;
        let mut stream = first.clone();
        stream.extend(encoded(&Message::Fault { text: "second".to_string() }));
        let events = decode_all(&stream);
        check!(
            events
                == vec![
                    Event::Dropped { bytes: first.len() },
                    Event::Message(Message::Fault { text: "second".to_string() })
                ]
        );
    }

    #[test]
    fn a_corrupted_payload_byte_is_caught_by_the_crc() {
        let mut frame = encoded(&Message::Input { bytes: vec![1, 2, 3, 4] });
        frame[HEADER_LEN + 2] ^= 0x80;
        check!(decode_all(&frame) == vec![Event::Dropped { bytes: frame.len() }]);
    }

    #[test]
    fn a_frame_hidden_inside_a_corrupted_one_is_still_found() {
        let inner = encoded(&Message::PushEnd);
        let mut stream = vec![0xFE, 0x4C, 0x03, 0x20, 0x00, 0x00, 0x00];
        stream.extend_from_slice(&inner);
        stream.extend_from_slice(&[0; 40]);
        check!(
            decode_all(&stream)
                == vec![Event::Dropped { bytes: 7 }, Event::Message(Message::PushEnd), Event::Dropped { bytes: 40 }]
        );
    }

    #[test]
    fn an_oversize_length_is_not_waited_for() {
        let mut stream = vec![0xFE, 0x4C, 0x05];
        stream.extend_from_slice(&(MAX_PAYLOAD as u32 + 1).to_le_bytes());
        stream.extend(encoded(&Message::Abort));
        let events = decode_all(&stream);
        check!(events.last() == Some(&Event::Message(Message::Abort)));
        check!(events.first() == Some(&Event::Dropped { bytes: 7 }));
    }

    #[test]
    fn a_payload_at_the_limit_round_trips_and_one_past_it_is_refused() {
        let full = Message::Input { bytes: vec![0xA5; MAX_PAYLOAD] };
        let frame = encoded(&full);
        check!(frame.len() == MAX_FRAME);
        check!(decode_all(&frame) == vec![Event::Message(full)]);

        let mut out = vec![1, 2, 3];
        let over = Message::Input { bytes: vec![0; MAX_PAYLOAD + 1] };
        check!(encode(&over, &mut out) == Err(EncodeError::TooLarge { len: MAX_PAYLOAD + 1 }));
        check!(out == [1, 2, 3], "a refused message leaves the buffer alone");
    }

    #[test]
    fn push_data_and_output_limits_match_their_headers() {
        let data = Message::PushData { offset: 0, bytes: vec![0; MAX_PUSH_DATA] };
        check!(encoded(&data).len() == MAX_FRAME);
        let data = Message::PushData { offset: 0, bytes: vec![0; MAX_PUSH_DATA + 1] };
        check!(encode(&data, &mut Vec::new()).is_err());
        let out = Message::Output { stream: Stream::Log, bytes: vec![0; MAX_OUTPUT] };
        check!(encoded(&out).len() == MAX_FRAME);
        let out = Message::Output { stream: Stream::Log, bytes: vec![0; MAX_OUTPUT + 1] };
        check!(encode(&out, &mut Vec::new()).is_err());
    }

    #[test]
    fn encode_output_matches_encoding_the_message() {
        let mut direct = Vec::new();
        assert!(let Ok(()) = encode_output(Stream::Data, &[1, 2, 3], &mut direct));
        check!(direct == encoded(&Message::Output { stream: Stream::Data, bytes: vec![1, 2, 3] }));
        check!(direct.len() == output_frame_len(3));
        check!(encode_output(Stream::Log, &vec![0; MAX_OUTPUT + 1], &mut Vec::new()).is_err());
    }

    #[test]
    fn short_strings_refuse_instead_of_truncating() {
        check!(ShortStr::new(&"n".repeat(MAX_SHORT)).is_ok());
        check!(ShortStr::new(&"n".repeat(MAX_SHORT + 1)) == Err(ShortStrError { len: MAX_SHORT + 1 }));
    }

    #[test]
    fn unknown_codes_survive_decoding_and_cannot_be_encoded() {
        let frame = {
            let body = [&[0x42u8][..], &3u32.to_le_bytes(), &[7, 8, 9]].concat();
            let mut frame = SYNC.to_vec();
            frame.extend_from_slice(&body);
            frame.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
            frame
        };
        let want = Message::Unknown { code: 0x42, len: 3 };
        check!(decode_all(&frame) == vec![Event::Message(want.clone())]);
        check!(encode(&want, &mut Vec::new()) == Err(EncodeError::Unencodable { code: 0x42 }));

        let msg = Message::PushBegin(PushBegin { kind: PushKind::Unknown(9), ..begin() });
        check!(decode_all(&encoded(&msg)) == vec![Event::Message(msg)]);
        let msg = Message::Output { stream: Stream::Unknown(9), bytes: vec![1] };
        check!(decode_all(&encoded(&msg)) == vec![Event::Message(msg)]);
    }

    #[rstest]
    #[case::hello_too_short(MessageType::Hello, &[1], PayloadError::Length { len: 1 })]
    #[case::hello_trailing(MessageType::Hello, &[1, 0, 0], PayloadError::Length { len: 3 })]
    #[case::empty_message_with_payload(MessageType::PushEnd, &[0], PayloadError::Length { len: 1 })]
    #[case::exited_short(MessageType::Exited, &[0; 11], PayloadError::Length { len: 11 })]
    #[case::name_overruns_payload(MessageType::Started, &[5, b'a'], PayloadError::Length { len: 2 })]
    #[case::fault_not_utf8(MessageType::Fault, &[0xFF, 0xFE], PayloadError::NotUtf8)]
    #[case::name_not_utf8(MessageType::Started, &[1, 0xFF], PayloadError::NotUtf8)]
    fn a_good_frame_with_a_bad_payload_is_reported_malformed(
        #[case] ty: MessageType,
        #[case] payload: &[u8],
        #[case] error: PayloadError,
    ) {
        let body = [&[ty.code()][..], &(payload.len() as u32).to_le_bytes(), payload].concat();
        let mut frame = SYNC.to_vec();
        frame.extend_from_slice(&body);
        frame.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
        check!(decode_all(&frame) == vec![Event::Malformed { ty, error }]);
    }

    #[test]
    fn exited_converts_to_and_from_a_duration() {
        let exited = Exited::new(0, Duration::from_micros(1_234_999));
        check!(exited.elapsed_ms == 1234);
        check!(exited.elapsed() == Duration::from_millis(1234));
        check!(Exited::new(0, Duration::from_secs(u64::from(u32::MAX))).elapsed_ms == u32::MAX);
    }

    #[test]
    fn push_begin_reports_its_watchdog_as_a_duration() {
        check!(begin().watchdog() == Duration::from_secs(300));
    }

    #[test]
    fn type_codes_round_trip() {
        for code in 0..=u8::MAX {
            check!(MessageType::from(code).code() == code);
            check!(PushKind::from(code).code() == code);
            check!(Stream::from(code).code() == code);
        }
    }
}
