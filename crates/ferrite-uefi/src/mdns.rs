//! mDNS responder for one IPv4-only hostname, `ferrite.local`.
//!
//! Answers `A` and `ANY` questions with the leased address and `AAAA` questions
//! with an `NSEC` record stating that only an `A` record exists, so resolvers
//! stop waiting for an IPv6 answer. Also builds the unsolicited announcements
//! sent when the lease changes. DNS-SD (`PTR`/`SRV`/`TXT`) is not served.

use alloc::vec::Vec;
use core::time::Duration;

use smoltcp::wire::Ipv4Address;
use snafu::{OptionExt as _, Snafu, ensure};

pub const MDNS_PORT: u16 = 5353;

/// The hostname we answer for.
pub const HOST: Name<'static> = {
    const LABELS: &[Label<'static>] = &[Label::from_static(b"ferrite"), Label::from_static(b"local")];
    match Name::new(LABELS) {
        Some(name) => name,
        None => panic!("hostname longer than 255 bytes"),
    }
};

const HEADER_LEN: usize = 12;
const MAX_POINTER_HOPS: usize = 16;

/// Top bit of a question's class: the querier accepts a unicast reply.
const QCLASS_UNICAST: u16 = 0x8000;
/// Top bit of a record's class: the record replaces cached ones.
const RCLASS_CACHE_FLUSH: u16 = 0x8000;

const FLAG_QR: u16 = 0x8000;
/// QR (response) + AA (authoritative).
const FLAGS_RESPONSE: u16 = 0x8400;

const TTL: Duration = Duration::from_secs(120);
/// Legacy resolvers cache what they get, so keep their copy short-lived.
const LEGACY_TTL: Duration = Duration::from_secs(10);

/// A record is multicast at most this often on the interface.
const MULTICAST_INTERVAL: Duration = Duration::from_secs(1);
const ANNOUNCE_COUNT: u8 = 2;
const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(1);

/// One DNS label: 1 to 63 bytes, compared ignoring ASCII case.
#[derive(Clone, Copy, Debug)]
pub struct Label<'a>(&'a [u8]);

impl<'a> Label<'a> {
    pub const MAX_LEN: usize = 63;

    #[must_use]
    pub const fn new(bytes: &'a [u8]) -> Option<Self> {
        if bytes.is_empty() || bytes.len() > Self::MAX_LEN { None } else { Some(Self(bytes)) }
    }

    /// For literals; an invalid label fails the build when used in a constant.
    const fn from_static(bytes: &'static [u8]) -> Label<'static> {
        match Label::new(bytes) {
            Some(label) => label,
            None => panic!("DNS label must be 1 to 63 bytes"),
        }
    }

    #[must_use]
    pub const fn as_bytes(self) -> &'a [u8] {
        self.0
    }
}

impl PartialEq for Label<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.0.eq_ignore_ascii_case(other.0)
    }
}

impl Eq for Label<'_> {}

/// A DNS name whose wire form is at most 255 bytes.
#[derive(Clone, Copy, Debug)]
pub struct Name<'a> {
    labels: &'a [Label<'a>],
}

impl<'a> Name<'a> {
    pub const MAX_WIRE_LEN: usize = 255;

    #[must_use]
    pub const fn new(labels: &'a [Label<'a>]) -> Option<Self> {
        let name = Self { labels };
        if name.wire_len() > Self::MAX_WIRE_LEN { None } else { Some(name) }
    }

    #[must_use]
    pub const fn labels(&self) -> &'a [Label<'a>] {
        self.labels
    }

    /// Length bytes, label bytes and the terminating zero.
    #[must_use]
    pub const fn wire_len(&self) -> usize {
        let mut len = 1;
        let mut i = 0;
        while i < self.labels.len() {
            len += 1 + self.labels[i].0.len();
            i += 1;
        }
        len
    }

    fn write(&self, out: &mut Vec<u8>) {
        for label in self.labels {
            out.push(label.0.len() as u8);
            out.extend_from_slice(label.0);
        }
        out.push(0);
    }
}

/// A resource record type from the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordType {
    A,
    Aaaa,
    Nsec,
    Any,
    Unknown(u16),
}

impl From<u16> for RecordType {
    fn from(raw: u16) -> Self {
        match raw {
            1 => Self::A,
            28 => Self::Aaaa,
            47 => Self::Nsec,
            255 => Self::Any,
            other => Self::Unknown(other),
        }
    }
}

impl From<RecordType> for u16 {
    fn from(rtype: RecordType) -> Self {
        match rtype {
            RecordType::A => 1,
            RecordType::Aaaa => 28,
            RecordType::Nsec => 47,
            RecordType::Any => 255,
            RecordType::Unknown(raw) => raw,
        }
    }
}

/// A record or question class, without the mDNS top bit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    In,
    Any,
    Unknown(u16),
}

impl Class {
    /// The class in the low 15 bits of `raw`.
    #[must_use]
    pub const fn from_wire(raw: u16) -> Self {
        match raw & 0x7FFF {
            1 => Self::In,
            255 => Self::Any,
            other => Self::Unknown(other),
        }
    }

    #[must_use]
    pub const fn to_wire(self) -> u16 {
        match self {
            Self::In => 1,
            Self::Any => 255,
            Self::Unknown(raw) => raw,
        }
    }
}

/// The header's opcode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Opcode {
    Query,
    Unknown(u8),
}

impl Opcode {
    const fn from_flags(flags: u16) -> Self {
        match ((flags >> 11) & 0xF) as u8 {
            0 => Self::Query,
            other => Self::Unknown(other),
        }
    }
}

/// The header's response code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rcode {
    NoError,
    Unknown(u8),
}

impl Rcode {
    const fn from_flags(flags: u16) -> Self {
        match (flags & 0xF) as u8 {
            0 => Self::NoError,
            other => Self::Unknown(other),
        }
    }
}

/// Why a packet could not be parsed. Offsets are from the start of the packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Snafu)]
pub enum ParseError {
    #[snafu(display("packet of {len} bytes is shorter than the DNS header"))]
    ShortHeader { len: usize },
    #[snafu(display("name at {at} runs past the end of the packet"))]
    TruncatedName { at: usize },
    #[snafu(display("label at {at} has reserved length byte {byte:#04x}"))]
    BadLabel { at: usize, byte: u8 },
    #[snafu(display("compression pointer at {at} does not point backwards"))]
    ForwardPointer { at: usize },
    #[snafu(display("name at {at} follows more than {MAX_POINTER_HOPS} pointers"))]
    TooManyPointers { at: usize },
    #[snafu(display("name at {at} is longer than {} bytes", Name::MAX_WIRE_LEN))]
    NameTooLong { at: usize },
    #[snafu(display("question at {at} is missing its type and class"))]
    TruncatedQuestion { at: usize },
}

/// Where the reply in `out` should go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Destination {
    /// The mDNS group, 224.0.0.251:5353.
    Multicast,
    /// Straight back to the packet's source address and port.
    Unicast,
}

/// What [`Responder::handle`] made of a well-formed packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handled {
    /// Another responder's answer (QR set), not a query.
    Response,
    /// A query we have nothing to say to: other names or types, a nonzero
    /// opcode or rcode, or no address leased yet.
    NoAnswer,
    /// Every record asked for was multicast less than a second ago.
    RateLimited,
    /// `out` holds a reply.
    Reply(Destination),
}

/// One parsed question.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Question {
    ours: bool,
    rtype: RecordType,
    class: Class,
    unicast_response: bool,
}

/// What the questions of one query asked about us.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Query {
    id: u16,
    want_a: bool,
    want_aaaa: bool,
    /// A question naming us had the unicast-response bit set.
    unicast: bool,
    /// Questions parsed, and the offset just past the last one.
    questions: u16,
    questions_end: usize,
}

impl Query {
    const fn asks_about_us(&self) -> bool {
        self.want_a || self.want_aaaa
    }

    fn add(&mut self, q: Question) {
        if !q.ours || !matches!(q.class, Class::In | Class::Any) {
            return;
        }
        let (a, aaaa) = match q.rtype {
            RecordType::A | RecordType::Any => (true, false),
            RecordType::Aaaa => (false, true),
            RecordType::Nsec | RecordType::Unknown(_) => (false, false),
        };
        self.want_a |= a;
        self.want_aaaa |= aaaa;
        self.unicast |= (a || aaaa) && q.unicast_response;
    }
}

/// Which records a reply carries and how they are framed.
struct ReplyShape<'p> {
    id: u16,
    /// Raw question section to echo, with its count.
    questions: Option<(&'p [u8], u16)>,
    a: bool,
    nsec: bool,
    ttl: Duration,
    cache_flush: bool,
}

/// Per-interface responder state: when each record was last multicast, and
/// pending announcements.
#[derive(Debug, Default)]
pub struct Responder {
    last_a: Option<Duration>,
    last_nsec: Option<Duration>,
    announcements_left: u8,
    next_announcement: Duration,
}

impl Responder {
    #[must_use]
    pub const fn new() -> Self {
        Self { last_a: None, last_nsec: None, announcements_left: 0, next_announcement: Duration::ZERO }
    }

    /// Answer `pkt`, received from `src_port` at `now`, writing any reply into `out`.
    pub fn handle(
        &mut self,
        pkt: &[u8],
        src_port: u16,
        now: Duration,
        ip: Option<Ipv4Address>,
        out: &mut Vec<u8>,
    ) -> Result<Handled, ParseError> {
        ensure!(pkt.len() >= HEADER_LEN, ShortHeaderSnafu { len: pkt.len() });
        let flags = be16(pkt, 2);
        if flags & FLAG_QR != 0 {
            return Ok(Handled::Response);
        }
        if Opcode::from_flags(flags) != Opcode::Query || Rcode::from_flags(flags) != Rcode::NoError {
            return Ok(Handled::NoAnswer);
        }
        let query = parse_questions(pkt)?;
        let Some(ip) = ip.filter(|_| query.asks_about_us()) else {
            return Ok(Handled::NoAnswer);
        };

        if src_port != MDNS_PORT {
            let shape = ReplyShape {
                id: query.id,
                questions: Some((&pkt[HEADER_LEN..query.questions_end], query.questions)),
                a: query.want_a,
                nsec: query.want_aaaa,
                ttl: LEGACY_TTL,
                cache_flush: false,
            };
            write_reply(out, &shape, ip);
            return Ok(Handled::Reply(Destination::Unicast));
        }
        if query.unicast {
            write_reply(out, &mdns_shape(query.want_a, query.want_aaaa), ip);
            return Ok(Handled::Reply(Destination::Unicast));
        }

        let a = query.want_a && may_multicast(self.last_a, now);
        let nsec = query.want_aaaa && may_multicast(self.last_nsec, now);
        if !a && !nsec {
            return Ok(Handled::RateLimited);
        }
        if a {
            self.last_a = Some(now);
        }
        if nsec {
            self.last_nsec = Some(now);
        }
        write_reply(out, &mdns_shape(a, nsec), ip);
        Ok(Handled::Reply(Destination::Multicast))
    }

    /// Queue announcements of a new or changed address, the first one due at `now`.
    pub fn schedule_announcements(&mut self, now: Duration) {
        self.announcements_left = ANNOUNCE_COUNT;
        self.next_announcement = now;
    }

    /// If an announcement is due, write it into `out` for multicast and return `true`.
    pub fn announcement(&mut self, now: Duration, ip: Ipv4Address, out: &mut Vec<u8>) -> bool {
        if self.announcements_left == 0 || now < self.next_announcement {
            return false;
        }
        self.announcements_left -= 1;
        self.next_announcement = now.saturating_add(ANNOUNCE_INTERVAL);
        self.last_a = Some(now);
        write_reply(out, &mdns_shape(true, false), ip);
        true
    }
}

fn may_multicast(last: Option<Duration>, now: Duration) -> bool {
    last.is_none_or(|last| now.saturating_sub(last) >= MULTICAST_INTERVAL)
}

const fn mdns_shape(a: bool, nsec: bool) -> ReplyShape<'static> {
    ReplyShape { id: 0, questions: None, a, nsec, ttl: TTL, cache_flush: true }
}

fn be16(pkt: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([pkt[at], pkt[at + 1]])
}

/// Parse the question section. A malformed question after one that named us
/// ends the section instead of discarding the packet.
fn parse_questions(pkt: &[u8]) -> Result<Query, ParseError> {
    let mut query = Query { id: be16(pkt, 0), questions_end: HEADER_LEN, ..Query::default() };
    let mut off = HEADER_LEN;
    for _ in 0..be16(pkt, 4) {
        let (question, next) = match parse_question(pkt, off) {
            Ok(parsed) => parsed,
            Err(_) if query.asks_about_us() => break,
            Err(e) => return Err(e),
        };
        off = next;
        query.questions += 1;
        query.questions_end = off;
        query.add(question);
    }
    Ok(query)
}

/// The question at `off`, and the offset past it.
fn parse_question(pkt: &[u8], off: usize) -> Result<(Question, usize), ParseError> {
    let (ours, next) = name_is_ours(pkt, off)?;
    ensure!(next + 4 <= pkt.len(), TruncatedQuestionSnafu { at: off });
    let qclass = be16(pkt, next + 2);
    let question = Question {
        ours,
        rtype: RecordType::from(be16(pkt, next)),
        class: Class::from_wire(qclass),
        unicast_response: qclass & QCLASS_UNICAST != 0,
    };
    Ok((question, next + 4))
}

fn name_is_ours(pkt: &[u8], start: usize) -> Result<(bool, usize), ParseError> {
    let host = HOST.labels();
    let mut index = 0;
    let mut matches = true;
    let next = walk_name(pkt, start, |label| {
        matches &= host.get(index).is_some_and(|h| *h == label);
        index += 1;
    })?;
    Ok((matches && index == host.len(), next))
}

/// Call `visit` for each label of the name at `start`, following compression
/// pointers. Returns the offset just past the name where it is stored.
fn walk_name<'p>(pkt: &'p [u8], start: usize, mut visit: impl FnMut(Label<'p>)) -> Result<usize, ParseError> {
    let mut pos = start;
    let mut end = None;
    let mut hops = 0;
    let mut wire_len = 1;
    loop {
        let byte = *pkt.get(pos).context(TruncatedNameSnafu { at: pos })?;
        match byte & 0xC0 {
            0x00 if byte == 0 => return Ok(end.unwrap_or(pos + 1)),
            0x00 => {
                let len = usize::from(byte);
                let text = pkt.get(pos + 1..pos + 1 + len).context(TruncatedNameSnafu { at: pos })?;
                wire_len += len + 1;
                ensure!(wire_len <= Name::MAX_WIRE_LEN, NameTooLongSnafu { at: start });
                visit(Label::new(text).context(BadLabelSnafu { at: pos, byte })?);
                pos += 1 + len;
            },
            0xC0 => {
                let low = *pkt.get(pos + 1).context(TruncatedNameSnafu { at: pos })?;
                let target = usize::from(u16::from_be_bytes([byte & 0x3F, low]));
                ensure!(target < pos, ForwardPointerSnafu { at: pos });
                hops += 1;
                ensure!(hops <= MAX_POINTER_HOPS, TooManyPointersSnafu { at: start });
                end.get_or_insert(pos + 2);
                pos = target;
            },
            _ => return BadLabelSnafu { at: pos, byte }.fail(),
        }
    }
}

/// Write a response: `A` as the answer when asked, `NSEC` as the answer for an
/// `AAAA`-only query or as additional data next to the `A` answer.
fn write_reply(out: &mut Vec<u8>, shape: &ReplyShape<'_>, ip: Ipv4Address) {
    let answers = u16::from(shape.a || shape.nsec);
    let additional = u16::from(shape.a && shape.nsec);
    let (questions, qdcount) = shape.questions.unwrap_or((&[], 0));

    out.clear();
    for field in [shape.id, FLAGS_RESPONSE, qdcount, answers, 0, additional] {
        out.extend_from_slice(&field.to_be_bytes());
    }
    out.extend_from_slice(questions);
    if shape.a {
        write_record_head(out, RecordType::A, shape, 4);
        out.extend_from_slice(&ip.octets());
    }
    if shape.nsec {
        // Next name is ourselves; the bitmap (window 0, one byte) lists only `A`.
        write_record_head(out, RecordType::Nsec, shape, HOST.wire_len() as u16 + 3);
        HOST.write(out);
        out.extend_from_slice(&[0x00, 0x01, 0x40]);
    }
}

fn write_record_head(out: &mut Vec<u8>, rtype: RecordType, shape: &ReplyShape<'_>, rdlength: u16) {
    let flush = if shape.cache_flush { RCLASS_CACHE_FLUSH } else { 0 };
    let ttl = u32::try_from(shape.ttl.as_secs()).unwrap_or(u32::MAX);
    HOST.write(out);
    out.extend_from_slice(&u16::from(rtype).to_be_bytes());
    out.extend_from_slice(&(Class::In.to_wire() | flush).to_be_bytes());
    out.extend_from_slice(&ttl.to_be_bytes());
    out.extend_from_slice(&rdlength.to_be_bytes());
}

#[cfg(test)]
mod tests;
