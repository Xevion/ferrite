//! The DbC benchmark wire format, shared by both ends of the link.
//!
//! Frame codec, stream checks, marker lines and the ready handshake. It depends on nothing
//! but `core`, `alloc` and `crc32fast`, and the byte layout is pinned by a fixed test vector.
//!
//! A frame is [`FRAME_LEN`] bytes: a little-endian header (magic, sequence number,
//! payload length, CRC-32 of the payload) followed by a payload that is a pure
//! function of the sequence number.

use alloc::vec::Vec;
use core::fmt;
use core::time::Duration;

pub const MAGIC: u32 = 0x4344_4246;
pub const HEADER_LEN: usize = 14;
pub const FRAME_LEN: usize = 1024;
pub const PAYLOAD_LEN: usize = FRAME_LEN - HEADER_LEN;
/// The line the desktop sends when it is ready for the next phase.
pub const READY_LINE: &[u8] = b"BENCH ready";

/// A line the target sends to announce a phase. The desktop sends [`READY_LINE`] before each one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Marker {
    /// The target sends `bytes` of frames of `frame_len` bytes.
    Source {
        bytes: u64,
        frame_len: u16,
    },
    SourceDone,
    /// The desktop sends frames for about `secs` seconds.
    Sink {
        secs: u32,
    },
    SinkDone,
    /// The desktop sends `size`-byte probes for about `secs` seconds and the target echoes them.
    Echo {
        secs: u32,
        size: u16,
    },
    EchoDone,
    Micro,
    Done,
}

impl fmt::Display for Marker {
    /// The line without its newline, like `BENCH source 4194304 1024`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source { bytes, frame_len } => write!(f, "BENCH source {bytes} {frame_len}"),
            Self::SourceDone => f.write_str("BENCH source done"),
            Self::Sink { secs } => write!(f, "BENCH sink {secs}"),
            Self::SinkDone => f.write_str("BENCH sink done"),
            Self::Echo { secs, size } => write!(f, "BENCH echo {secs} {size}"),
            Self::EchoDone => f.write_str("BENCH echo done"),
            Self::Micro => f.write_str("BENCH micro"),
            Self::Done => f.write_str("BENCH done"),
        }
    }
}

impl Marker {
    /// Parse a marker line, with or without its trailing newline; `None` for anything else.
    #[must_use]
    pub fn parse(line: &str) -> Option<Self> {
        let mut words = line.split_whitespace();
        if words.next()? != "BENCH" {
            return None;
        }
        let phase = words.next()?;
        // `done` or the phase's first number; anything else is not a marker.
        let first = words.next();
        let number = |word: Option<&str>| word?.parse::<u64>().ok();
        let marker = match (phase, first) {
            ("source", Some("done")) => Self::SourceDone,
            ("source", bytes) => {
                Self::Source { bytes: number(bytes)?, frame_len: u16::try_from(number(words.next())?).ok()? }
            },
            ("sink", Some("done")) => Self::SinkDone,
            ("sink", secs) => Self::Sink { secs: u32::try_from(number(secs)?).ok()? },
            ("echo", Some("done")) => Self::EchoDone,
            ("echo", secs) => Self::Echo {
                secs: u32::try_from(number(secs)?).ok()?,
                size: u16::try_from(number(words.next())?).ok()?,
            },
            ("micro", None) => Self::Micro,
            ("done", None) => Self::Done,
            _ => return None,
        };
        words.next().is_none().then_some(marker)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub seq: u32,
    pub len: u16,
    pub crc: u32,
}

impl Header {
    #[must_use]
    pub fn for_payload(seq: u32, payload: &[u8]) -> Self {
        Self { seq, len: payload.len() as u16, crc: crc32fast::hash(payload) }
    }

    #[must_use]
    pub fn to_bytes(self) -> [u8; HEADER_LEN] {
        let mut out = [0; HEADER_LEN];
        out[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        out[4..8].copy_from_slice(&self.seq.to_le_bytes());
        out[8..10].copy_from_slice(&self.len.to_le_bytes());
        out[10..14].copy_from_slice(&self.crc.to_le_bytes());
        out
    }

    /// `None` when `bytes` is too short, the magic is wrong or the length cannot fit a frame.
    #[must_use]
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let bytes: &[u8; HEADER_LEN] = bytes.get(..HEADER_LEN)?.try_into().ok()?;
        let word = |at: usize| u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
        let len = u16::from_le_bytes([bytes[8], bytes[9]]);
        (word(0) == MAGIC && usize::from(len) <= PAYLOAD_LEN).then(|| Self { seq: word(4), len, crc: word(10) })
    }
}

/// The payload of frame `seq`: byte `i` is `seq * 131 + i * 7` modulo 256.
pub fn fill_payload(seq: u32, out: &mut [u8]) {
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = seq.wrapping_mul(131).wrapping_add((i as u32).wrapping_mul(7)) as u8;
    }
}

/// Write frame `seq` into `out`.
pub fn encode_frame(seq: u32, out: &mut [u8; FRAME_LEN]) {
    let (head, payload) = out.split_at_mut(HEADER_LEN);
    fill_payload(seq, payload);
    head.copy_from_slice(&Header::for_payload(seq, payload).to_bytes());
}

/// Frames for any sequence number with no per-frame payload or CRC work.
///
/// The payload depends only on `seq` modulo 256, so 256 encoded frames cover every
/// sequence number once the header's `seq` field is patched.
pub struct FrameSource {
    frames: Vec<[u8; FRAME_LEN]>,
}

impl FrameSource {
    #[must_use]
    pub fn new() -> Self {
        let mut frames = alloc::vec![[0; FRAME_LEN]; 256];
        for (seq, frame) in (0..).zip(&mut frames) {
            encode_frame(seq, frame);
        }
        Self { frames }
    }

    /// Frame `seq`, identical to what [`encode_frame`] produces.
    pub fn frame(&mut self, seq: u32) -> &[u8; FRAME_LEN] {
        let frame = &mut self.frames[(seq % 256) as usize];
        frame[4..8].copy_from_slice(&seq.to_le_bytes());
        frame
    }
}

impl Default for FrameSource {
    fn default() -> Self {
        Self::new()
    }
}

/// Frames `0..frames` back to back.
#[must_use]
pub fn source_stream(frames: u32) -> Vec<u8> {
    let mut stream = alloc::vec![0; frames as usize * FRAME_LEN];
    for (seq, frame) in (0..frames).zip(stream.as_chunks_mut::<FRAME_LEN>().0) {
        encode_frame(seq, frame);
    }
    stream
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameEvent {
    Valid {
        seq: u32,
    },
    BadCrc {
        seq: u32,
    },
    /// Bytes dropped while looking for the next header.
    Skipped(usize),
}

/// Reassembles frames from a byte stream cut at arbitrary points, resynchronising on the
/// magic after garbage.
pub struct FrameReader {
    buf: [u8; FRAME_LEN],
    have: usize,
}

impl Default for FrameReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameReader {
    #[must_use]
    pub const fn new() -> Self {
        Self { buf: [0; FRAME_LEN], have: 0 }
    }

    pub fn feed(&mut self, mut bytes: &[u8], mut on: impl FnMut(FrameEvent)) {
        while !bytes.is_empty() {
            let take = self.want().saturating_sub(self.have).min(bytes.len());
            self.buf[self.have..self.have + take].copy_from_slice(&bytes[..take]);
            self.have += take;
            bytes = &bytes[take..];
            self.advance(&mut on);
        }
    }

    /// Bytes the frame in progress needs in total.
    fn want(&self) -> usize {
        if self.have < HEADER_LEN {
            return HEADER_LEN;
        }
        Header::parse(&self.buf).map_or(HEADER_LEN, |h| HEADER_LEN + usize::from(h.len))
    }

    /// Drop bytes until the buffer starts with a valid header, and report a finished frame.
    fn advance(&mut self, on: &mut impl FnMut(FrameEvent)) {
        while self.have >= HEADER_LEN {
            let Some(header) = Header::parse(&self.buf) else {
                self.buf.copy_within(1..self.have, 0);
                self.have -= 1;
                on(FrameEvent::Skipped(1));
                continue;
            };
            let total = HEADER_LEN + usize::from(header.len);
            if self.have < total {
                return;
            }
            let intact = crc32fast::hash(&self.buf[HEADER_LEN..total]) == header.crc;
            on(if intact { FrameEvent::Valid { seq: header.seq } } else { FrameEvent::BadCrc { seq: header.seq } });
            self.have = 0;
            return;
        }
    }
}

/// What the sink phase saw.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SinkStats {
    pub frames_ok: u32,
    pub crc_failures: u32,
    /// Times a frame arrived with a sequence number other than the expected one.
    pub gaps: u32,
    /// Frames skipped over by those gaps.
    pub missing: u32,
    pub resync_bytes: usize,
    expected: Option<u32>,
}

impl SinkStats {
    pub fn observe(&mut self, event: FrameEvent) {
        let seq = match event {
            FrameEvent::Valid { seq } => {
                self.frames_ok += 1;
                seq
            },
            FrameEvent::BadCrc { seq } => {
                self.crc_failures += 1;
                seq
            },
            FrameEvent::Skipped(n) => {
                self.resync_bytes += n;
                return;
            },
        };
        if let Some(expected) = self.expected {
            let ahead = seq.wrapping_sub(expected);
            if ahead != 0 {
                self.gaps += 1;
                if ahead < 1 << 31 {
                    self.missing = self.missing.saturating_add(ahead);
                }
            }
        }
        self.expected = Some(seq.wrapping_add(1));
    }
}

/// Spots the desktop's ready line in a byte stream cut at arbitrary points.
#[derive(Clone, Debug, Default)]
pub struct ReadyDetector {
    line: [u8; READY_LINE.len()],
    len: usize,
    overflowed: bool,
}

impl ReadyDetector {
    /// Whether a complete ready line ended inside `bytes`. Anything else on a line is ignored.
    pub fn feed(&mut self, bytes: &[u8]) -> bool {
        let mut ready = false;
        for &byte in bytes {
            match byte {
                b'\n' => {
                    ready |= !self.overflowed && &self.line[..self.len] == READY_LINE;
                    self.len = 0;
                    self.overflowed = false;
                },
                b'\r' => {},
                _ if self.len < READY_LINE.len() => {
                    self.line[self.len] = byte;
                    self.len += 1;
                },
                _ => self.overflowed = true,
            }
        }
        ready
    }
}

/// The nearest-rank percentile of an ascending slice; `None` when it is empty.
#[must_use]
pub fn percentile(sorted: &[Duration], pct: usize) -> Option<Duration> {
    let last = sorted.len().checked_sub(1)?;
    sorted.get((sorted.len() * pct).div_ceil(100).saturating_sub(1).min(last)).copied()
}

#[cfg(test)]
mod tests {
    use core::fmt::Write as _;

    use assert2::{assert, check};
    use rstest::rstest;

    use super::*;

    #[rstest]
    fn frame_source_matches_the_reference_encoder(#[values(0, 1, 255, 256, 257, 70_000, u32::MAX)] seq: u32) {
        let mut expected = [0; FRAME_LEN];
        encode_frame(seq, &mut expected);
        let mut source = FrameSource::new();
        source.frame(seq.wrapping_add(1));
        check!(source.frame(seq) == &expected);
    }

    /// Pinned bytes: a change here breaks the deployed other end of the link.
    #[rstest]
    #[case::first(0, "4642444300000000f20302724b76")]
    #[case::second(1, "4642444301000000f2039d4f7912")]
    #[case::last_of_four_mib(4095, "46424443ff0f0000f203eebc54d3")]
    fn header_bytes_match_the_desktop_tool(#[case] seq: u32, #[case] want: &str) {
        let mut frame = [0; FRAME_LEN];
        encode_frame(seq, &mut frame);
        let hex = frame[..HEADER_LEN].iter().fold(alloc::string::String::new(), |mut hex, b| {
            write!(hex, "{b:02x}").expect("writing to a String");
            hex
        });
        check!(hex == want);
    }

    #[rstest]
    #[case::source("BENCH source 4194304 1024", Some(Marker::Source { bytes: 4_194_304, frame_len: 1024 }))]
    #[case::source_done("BENCH source done\n", Some(Marker::SourceDone))]
    #[case::sink("BENCH sink 10", Some(Marker::Sink { secs: 10 }))]
    #[case::echo("BENCH echo 10 64\r\n", Some(Marker::Echo { secs: 10, size: 64 }))]
    #[case::micro("BENCH micro", Some(Marker::Micro))]
    #[case::done("BENCH done", Some(Marker::Done))]
    #[case::ready_is_not_a_marker("BENCH ready", None)]
    #[case::missing_size("BENCH echo 10", None)]
    #[case::oversized_frame("BENCH source 1 70000", None)]
    #[case::trailing_word("BENCH sink 10 now", None)]
    #[case::source_without_numbers("BENCH source", None)]
    #[case::sink_with_garbage("BENCH sink soon", None)]
    #[case::echo_done_with_extra("BENCH echo done 5", None)]
    #[case::micro_with_extra("BENCH micro 1", None)]
    #[case::other_text("hello", None)]
    fn marker_lines_parse(#[case] line: &str, #[case] want: Option<Marker>) {
        check!(Marker::parse(line) == want);
    }

    #[test]
    fn markers_display_as_their_line() {
        check!(
            alloc::format!("{}", Marker::Source { bytes: 4_194_304, frame_len: 1024 }) == "BENCH source 4194304 1024"
        );
        check!(alloc::format!("{}", Marker::EchoDone) == "BENCH echo done");
    }

    #[test]
    fn payload_is_a_pure_function_of_the_sequence() {
        let mut payload = [0; 8];
        fill_payload(1, &mut payload);
        check!(payload == [0x83, 0x8a, 0x91, 0x98, 0x9f, 0xa6, 0xad, 0xb4]);
    }

    fn feed_all(stream: &[u8], chunk: usize) -> (SinkStats, usize) {
        let mut reader = FrameReader::new();
        let mut stats = SinkStats::default();
        for part in stream.chunks(chunk) {
            reader.feed(part, |event| stats.observe(event));
        }
        (stats, stats.resync_bytes)
    }

    #[rstest]
    #[case::one_byte_at_a_time(1)]
    #[case::odd_chunks(7)]
    #[case::packet_sized(1024)]
    #[case::whole(4 * FRAME_LEN)]
    fn reader_reassembles_frames_cut_anywhere(#[case] chunk: usize) {
        let (stats, skipped) = feed_all(&source_stream(4), chunk);
        check!((stats.frames_ok, stats.crc_failures, stats.gaps, skipped) == (4, 0, 0, 0));
    }

    #[test]
    fn reader_resynchronises_after_garbage_and_flags_corruption() {
        let mut stream = alloc::vec![1, 2, 3];
        stream.extend_from_slice(&source_stream(3));
        stream[3 + FRAME_LEN + HEADER_LEN + 5] ^= 0xff;
        let (stats, skipped) = feed_all(&stream, 100);
        check!(skipped == 3);
        check!((stats.frames_ok, stats.crc_failures, stats.gaps) == (2, 1, 0));
    }

    #[test]
    fn sink_counts_gaps_and_missing_frames() {
        let mut stats = SinkStats::default();
        for seq in [0, 1, 3, 4, 4] {
            stats.observe(FrameEvent::Valid { seq });
        }
        check!((stats.gaps, stats.missing) == (2, 1), "a skipped frame and a repeat");
    }

    #[test]
    fn ready_line_is_found_across_chunks_and_among_noise() {
        let mut detector = ReadyDetector::default();
        check!(!detector.feed(b"BENCH ready"));
        check!(detector.feed(b"\r\n"));
        check!(!detector.feed(b"BENCH readyx\n"), "a longer line is not the ready line");
        check!(!detector.feed(b"BENCH read\n"));
        check!(detector.feed(b"junk\nBENCH ready\nmore"));
    }

    #[test]
    fn percentiles_use_the_nearest_rank() {
        let d: Vec<_> = (1..=100).map(Duration::from_micros).collect();
        assert!(let Some(p50) = percentile(&d, 50));
        check!(p50 == Duration::from_micros(50));
        check!(percentile(&d, 99) == Some(Duration::from_micros(99)));
        check!(percentile(&d, 100) == Some(Duration::from_micros(100)));
        check!(percentile(&[], 50) == None);
    }
}
