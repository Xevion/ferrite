//! Netlink framing: `nlmsghdr` messages packed into a datagram, and `nlattr` TLVs inside them.
//!
//! Integers are in host byte order, as the kernel writes them; everything here runs on x86-64. A
//! message's length covers its header and payload, and the next message starts at the length rounded
//! up to 4. An attribute is `len: u16, type: u16` and a payload, padded to 4 the same way.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use snafu::{Snafu, ensure};

/// `NLMSG_HDRLEN`: `len: u32, type: u16, flags: u16, seq: u32, pid: u32`.
pub const HEADER_LEN: usize = 16;
/// `NLA_HDRLEN`.
pub const ATTR_HEADER_LEN: usize = 4;
/// `NLA_F_NESTED`: the payload is itself attributes.
const ATTR_NESTED: u16 = 0x8000;
/// `NLA_F_NET_BYTEORDER`.
const ATTR_NET_BYTEORDER: u16 = 0x4000;
/// `NLA_TYPE_MASK`.
const ATTR_TYPE_MASK: u16 = !(ATTR_NESTED | ATTR_NET_BYTEORDER);
/// `NLMSGERR_ATTR_MSG`: the extended ack's text.
const ERR_ATTR_MSG: u16 = 1;

/// `NLMSG_ALIGN` and `NLA_ALIGN`.
pub const fn align(len: usize) -> usize {
    (len + 3) & !3
}

#[derive(Debug, Clone, PartialEq, Eq, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum DecodeError {
    #[snafu(display("a message header at offset {offset} claims {len} bytes, {left} are left"))]
    MessageLength { offset: usize, len: usize, left: usize },
    #[snafu(display("an attribute at offset {offset} claims {len} bytes, {left} are left"))]
    AttrLength { offset: usize, len: usize, left: usize },
    #[snafu(display("attribute {kind} has {len} bytes, {want} expected"))]
    AttrSize { kind: u16, len: usize, want: usize },
    #[snafu(display("attribute {kind} is not a NUL-terminated UTF-8 string"))]
    AttrString { kind: u16 },
    #[snafu(display("a payload of {len} bytes is shorter than its {want}-byte header"))]
    Short { len: usize, want: usize },
}

/// `nlmsghdr.nlmsg_type`: the four control messages, or a protocol's own (a generic netlink family's id).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageType {
    Noop,
    Error,
    Done,
    Overrun,
    Protocol(u16),
}

impl MessageType {
    pub const fn from_raw(raw: u16) -> Self {
        match raw {
            1 => Self::Noop,
            2 => Self::Error,
            3 => Self::Done,
            4 => Self::Overrun,
            other => Self::Protocol(other),
        }
    }

    pub const fn raw(self) -> u16 {
        match self {
            Self::Noop => 1,
            Self::Error => 2,
            Self::Done => 3,
            Self::Overrun => 4,
            Self::Protocol(raw) => raw,
        }
    }
}

/// `nlmsghdr.nlmsg_flags`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flags(pub u16);

impl Flags {
    pub const REQUEST: Self = Self(0x1);
    pub const MULTI: Self = Self(0x2);
    pub const ACK: Self = Self(0x4);
    /// `NLM_F_ROOT | NLM_F_MATCH`.
    pub const DUMP: Self = Self(0x300);
    /// On an error message: the echoed request is only its header.
    pub const CAPPED: Self = Self(0x100);
    /// On an error message: extended-ack attributes follow the echoed request.
    pub const ACK_TLVS: Self = Self(0x200);

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    #[must_use]
    pub const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// One message of a datagram, borrowing its payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Message<'a> {
    pub kind: MessageType,
    pub flags: Flags,
    pub seq: u32,
    pub port: u32,
    pub payload: &'a [u8],
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_ne_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// The messages packed in one received datagram, in order.
pub fn messages(datagram: &[u8]) -> Messages<'_> {
    Messages { bytes: datagram, offset: 0 }
}

pub struct Messages<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Iterator for Messages<'a> {
    type Item = Result<Message<'a>, DecodeError>;

    fn next(&mut self) -> Option<Self::Item> {
        let rest = self.bytes.get(self.offset..).filter(|rest| !rest.is_empty())?;
        let offset = self.offset;
        if rest.len() < HEADER_LEN {
            self.offset = self.bytes.len();
            return Some(Err(DecodeError::MessageLength { offset, len: HEADER_LEN, left: rest.len() }));
        }
        let len = u32_at(rest, 0) as usize;
        if len < HEADER_LEN || len > rest.len() {
            self.offset = self.bytes.len();
            return Some(Err(DecodeError::MessageLength { offset, len, left: rest.len() }));
        }
        self.offset += align(len).min(rest.len());
        Some(Ok(Message {
            kind: MessageType::from_raw(u16_at(rest, 4)),
            flags: Flags(u16_at(rest, 6)),
            seq: u32_at(rest, 8),
            port: u32_at(rest, 12),
            payload: &rest[HEADER_LEN..len],
        }))
    }
}

/// An `NLMSG_ERROR` payload: 0 for an ack, else a negative errno, with the extended ack's text if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorMessage {
    pub code: i32,
    pub text: Option<String>,
}

impl ErrorMessage {
    pub fn parse(message: &Message<'_>) -> Result<Self, DecodeError> {
        let payload = message.payload;
        ensure!(payload.len() >= 4 + HEADER_LEN, ShortSnafu { len: payload.len(), want: 4 + HEADER_LEN });
        let code = u32_at(payload, 0).cast_signed();
        let echoed = if message.flags.contains(Flags::CAPPED) { HEADER_LEN } else { u32_at(payload, 4) as usize };
        let mut text = None;
        if message.flags.contains(Flags::ACK_TLVS)
            && let Some(tlvs) = payload.get(4 + align(echoed)..)
        {
            for attr in attrs(tlvs) {
                let attr = attr?;
                if attr.kind == ERR_ATTR_MSG {
                    text = Some(String::from(attr.str()?));
                }
            }
        }
        Ok(Self { code, text })
    }
}

/// One attribute, borrowing its payload; `kind` has the nested and byte-order flags masked off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attr<'a> {
    pub kind: u16,
    pub nested: bool,
    pub payload: &'a [u8],
}

impl<'a> Attr<'a> {
    fn fixed<const N: usize>(&self) -> Result<[u8; N], DecodeError> {
        self.payload.try_into().map_err(|_| DecodeError::AttrSize { kind: self.kind, len: self.payload.len(), want: N })
    }

    pub fn u8(&self) -> Result<u8, DecodeError> {
        self.fixed::<1>().map(|b| b[0])
    }

    pub fn u16(&self) -> Result<u16, DecodeError> {
        self.fixed().map(u16::from_ne_bytes)
    }

    pub fn u32(&self) -> Result<u32, DecodeError> {
        self.fixed().map(u32::from_ne_bytes)
    }

    pub fn i32(&self) -> Result<i32, DecodeError> {
        self.fixed().map(i32::from_ne_bytes)
    }

    pub fn array<const N: usize>(&self) -> Result<[u8; N], DecodeError> {
        self.fixed()
    }

    /// An `NLA_NUL_STRING`, without its terminator.
    pub fn str(&self) -> Result<&'a str, DecodeError> {
        let error = DecodeError::AttrString { kind: self.kind };
        let (last, text) = self.payload.split_last().ok_or_else(|| error.clone())?;
        if *last != 0 {
            return Err(error);
        }
        core::str::from_utf8(text).map_err(|_| error)
    }

    /// The attributes nested in this one.
    pub fn attrs(&self) -> Attrs<'a> {
        attrs(self.payload)
    }
}

/// The attributes in `bytes`, in order.
pub fn attrs(bytes: &[u8]) -> Attrs<'_> {
    Attrs { bytes, offset: 0 }
}

pub struct Attrs<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Iterator for Attrs<'a> {
    type Item = Result<Attr<'a>, DecodeError>;

    fn next(&mut self) -> Option<Self::Item> {
        let rest = self.bytes.get(self.offset..).filter(|rest| !rest.is_empty())?;
        let offset = self.offset;
        let len = if rest.len() >= ATTR_HEADER_LEN { usize::from(u16_at(rest, 0)) } else { ATTR_HEADER_LEN };
        if len < ATTR_HEADER_LEN || len > rest.len() {
            self.offset = self.bytes.len();
            return Some(Err(DecodeError::AttrLength { offset, len, left: rest.len() }));
        }
        let raw = u16_at(rest, 2);
        self.offset += align(len).min(rest.len());
        Some(Ok(Attr {
            kind: raw & ATTR_TYPE_MASK,
            nested: raw & ATTR_NESTED != 0,
            payload: &rest[ATTR_HEADER_LEN..len],
        }))
    }
}

/// Builds one request message: header, then a protocol header, then attributes.
pub struct Builder {
    bytes: Vec<u8>,
}

impl Builder {
    pub fn new(kind: MessageType, flags: Flags, seq: u32) -> Self {
        let mut bytes = Vec::with_capacity(256);
        bytes.extend_from_slice(&0_u32.to_ne_bytes());
        bytes.extend_from_slice(&kind.raw().to_ne_bytes());
        bytes.extend_from_slice(&flags.0.to_ne_bytes());
        bytes.extend_from_slice(&seq.to_ne_bytes());
        bytes.extend_from_slice(&0_u32.to_ne_bytes());
        Self { bytes }
    }

    /// Raw bytes at the current position, padded to 4.
    #[must_use]
    pub fn raw(mut self, bytes: &[u8]) -> Self {
        self.bytes.extend_from_slice(bytes);
        self.pad();
        self
    }

    fn pad(&mut self) {
        self.bytes.resize(align(self.bytes.len()), 0);
    }

    /// # Panics
    /// If the payload does not fit an attribute's 16-bit length.
    #[must_use]
    pub fn attr(mut self, kind: u16, payload: &[u8]) -> Self {
        let len =
            u16::try_from(ATTR_HEADER_LEN + payload.len()).unwrap_or_else(|_| panic!("attribute {kind} too long"));
        self.bytes.extend_from_slice(&len.to_ne_bytes());
        self.bytes.extend_from_slice(&kind.to_ne_bytes());
        self.bytes.extend_from_slice(payload);
        self.pad();
        self
    }

    #[must_use]
    pub fn u32(self, kind: u16, value: u32) -> Self {
        self.attr(kind, &value.to_ne_bytes())
    }

    /// A NUL-terminated string attribute.
    #[must_use]
    pub fn str(self, kind: u16, value: &str) -> Self {
        let mut payload = Vec::with_capacity(value.len() + 1);
        payload.extend_from_slice(value.as_bytes());
        payload.push(0);
        self.attr(kind, &payload)
    }

    /// A nested attribute holding what `inner` adds.
    ///
    /// # Panics
    /// If the nested attributes do not fit a 16-bit length.
    #[must_use]
    pub fn nest(mut self, kind: u16, inner: impl FnOnce(Self) -> Self) -> Self {
        let start = self.bytes.len();
        self.bytes.extend_from_slice(&[0; ATTR_HEADER_LEN]);
        self = inner(self);
        let len = u16::try_from(self.bytes.len() - start).unwrap_or_else(|_| panic!("nest {kind} too long"));
        self.bytes[start..start + 2].copy_from_slice(&len.to_ne_bytes());
        self.bytes[start + 2..start + 4].copy_from_slice(&(kind | ATTR_NESTED).to_ne_bytes());
        self
    }

    /// The finished message, its length filled in.
    pub fn finish(mut self) -> Vec<u8> {
        let len = self.bytes.len() as u32;
        self.bytes[..4].copy_from_slice(&len.to_ne_bytes());
        self.bytes
    }
}

/// A negative errno the kernel answered with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Errno(pub i32);

impl fmt::Display for Errno {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match -self.0 {
            1 => "EPERM",
            2 => "ENOENT",
            11 => "EAGAIN",
            12 => "ENOMEM",
            16 => "EBUSY",
            19 => "ENODEV",
            22 => "EINVAL",
            95 => "EOPNOTSUPP",
            105 => "ENOBUFS",
            114 => "EALREADY",
            _ => return write!(f, "errno {}", self.0),
        };
        write!(f, "-{name}")
    }
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};

    use super::*;

    #[test]
    fn attributes_are_padded_to_four_and_the_next_one_starts_after_the_padding() {
        // A 5-byte payload ("abcd\0") pads to 8; a u16 after it starts at 12.
        let bytes = [9, 0, 2, 0, b'a', b'b', b'c', b'd', 0, 0xee, 0xee, 0xee, 6, 0, 3, 0, 0x34, 0x12, 0, 0];
        let found: Vec<Attr<'_>> = attrs(&bytes).collect::<Result<_, _>>().unwrap();
        check!(found.len() == 2);
        check!(found[0].str() == Ok("abcd"));
        check!(found[1].kind == 3);
        check!(found[1].u16() == Ok(0x1234));
    }

    #[test]
    fn the_nested_flag_is_masked_out_of_the_type() {
        let bytes = [8, 0, 7, 0x80, 4, 0, 1, 0];
        let outer = attrs(&bytes).next().unwrap().unwrap();
        check!(outer.kind == 7);
        check!(outer.nested);
        let inner: Vec<Attr<'_>> = outer.attrs().collect::<Result<_, _>>().unwrap();
        check!(inner == [Attr { kind: 1, nested: false, payload: &[] }]);
    }

    #[test]
    fn an_attribute_longer_than_what_is_left_is_an_error_and_ends_the_walk() {
        let bytes = [12, 0, 1, 0, 0, 0, 0, 0];
        let mut walk = attrs(&bytes);
        check!(walk.next() == Some(Err(DecodeError::AttrLength { offset: 0, len: 12, left: 8 })));
        check!(walk.next().is_none());
    }

    #[test]
    fn a_trailing_fragment_shorter_than_a_header_is_an_error() {
        let bytes = [4, 0, 1, 0, 0xff, 0xff];
        let found: Vec<_> = attrs(&bytes).collect();
        check!(found.len() == 2);
        check!(found[1] == Err(DecodeError::AttrLength { offset: 4, len: 4, left: 2 }));
    }

    #[test]
    fn a_datagram_carries_several_messages_each_aligned() {
        // A 17-byte message (one payload byte) pads to 20; NLMSG_DONE follows with a 4-byte error.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&17_u32.to_ne_bytes());
        bytes.extend_from_slice(&0x1c_u16.to_ne_bytes());
        bytes.extend_from_slice(&2_u16.to_ne_bytes());
        bytes.extend_from_slice(&5_u32.to_ne_bytes());
        bytes.extend_from_slice(&77_u32.to_ne_bytes());
        bytes.extend_from_slice(&[0xab, 0, 0, 0]);
        bytes.extend_from_slice(&20_u32.to_ne_bytes());
        bytes.extend_from_slice(&3_u16.to_ne_bytes());
        bytes.extend_from_slice(&2_u16.to_ne_bytes());
        bytes.extend_from_slice(&5_u32.to_ne_bytes());
        bytes.extend_from_slice(&77_u32.to_ne_bytes());
        bytes.extend_from_slice(&0_i32.to_ne_bytes());
        let found: Vec<Message<'_>> = messages(&bytes).collect::<Result<_, _>>().unwrap();
        check!(found.len() == 2);
        check!(found[0].kind == MessageType::Protocol(0x1c));
        check!(found[0].payload == [0xab]);
        check!(found[0].flags.contains(Flags::MULTI));
        check!(found[1].kind == MessageType::Done);
    }

    #[test]
    fn an_error_message_with_extended_ack_carries_its_text() {
        // NLMSG_ERROR, capped, -EINVAL, the echoed 16-byte header, then NLMSGERR_ATTR_MSG "bad".
        let mut payload = Vec::new();
        payload.extend_from_slice(&(-22_i32).to_ne_bytes());
        payload.extend_from_slice(&[0; HEADER_LEN]);
        payload.extend_from_slice(&[8, 0, 1, 0, b'b', b'a', b'd', 0]);
        let message = Message {
            kind: MessageType::Error,
            flags: Flags::CAPPED.with(Flags::ACK_TLVS),
            seq: 1,
            port: 0,
            payload: &payload,
        };
        assert!(let Ok(error) = ErrorMessage::parse(&message));
        check!(error.code == -22);
        check!(error.text.as_deref() == Some("bad"));
    }
}
