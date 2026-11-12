//! The four benchmark phases as the desktop plays them.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use ferrite_wire::bench::{FRAME_LEN, FrameReader, Marker, SinkStats, encode_frame};

use super::BenchError;
use super::peer::Peer;
use super::report::{EchoResult, SinkResult, SourceResult};

/// How long the source stream may go without a byte.
pub const STREAM_IDLE: Duration = Duration::from_secs(10);
pub const SOURCE_LIMIT: Duration = Duration::from_secs(90);
/// How long past the announced window the target gets to send its end marker.
pub const DONE_SLACK: Duration = Duration::from_secs(20);
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
/// Echo probing stops this much early so the last probe is not sent after the target stopped.
pub const ECHO_MARGIN: Duration = Duration::from_millis(200);
const SEND_SLACK: Duration = Duration::from_secs(10);
const BATCH_FRAMES: usize = 16;
const MAX_RTT_SAMPLES: usize = 1 << 20;

/// Checks a frame stream that ends with the source-done marker line.
///
/// The marker is cut out before the frame reader sees it, so it is not counted as garbage.
pub struct SourceChecker {
    reader: FrameReader,
    stats: SinkStats,
    held: Vec<u8>,
    marker: Vec<u8>,
    fed: u64,
    done: bool,
}

impl SourceChecker {
    pub fn new() -> Self {
        Self {
            reader: FrameReader::new(),
            stats: SinkStats::default(),
            held: Vec::new(),
            marker: Marker::SourceDone.to_string().into_bytes(),
            fed: 0,
            done: false,
        }
    }

    pub const fn done(&self) -> bool {
        self.done
    }

    /// Check `chunk`. Once the end marker is seen, returns the bytes that followed it.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.held.extend_from_slice(chunk);
        if let Some(at) = self.held.windows(self.marker.len()).position(|w| w == self.marker) {
            let rest = self.held.split_off(at + self.marker.len());
            self.held.truncate(at);
            self.release(self.held.len());
            self.done = true;
            return rest;
        }
        let keep = (self.marker.len() - 1).min(self.held.len());
        self.release(self.held.len() - keep);
        Vec::new()
    }

    /// Check bytes still held back, when the stream ended without the marker.
    pub fn finish(&mut self) {
        self.release(self.held.len());
    }

    fn release(&mut self, n: usize) {
        let stats = &mut self.stats;
        self.reader.feed(&self.held[..n], |event| stats.observe(event));
        self.fed += n as u64;
        self.held.drain(..n);
    }
}

pub fn source<L: Read + Write>(
    peer: &mut Peer<'_, L>,
    announced_bytes: u64,
    frame_len: u16,
    idle: Duration,
    limit: Duration,
) -> Result<SourceResult, BenchError> {
    let t0 = Instant::now();
    let mut checker = SourceChecker::new();
    let mut leftover = checker.feed(&peer.take_pending());
    let mut last_data = t0;
    let mut buf = [0; 16 * 1024];
    while !checker.done() && t0.elapsed() < limit && last_data.elapsed() < idle {
        peer.check_stop()?;
        if let Some(n) = peer.read_into(&mut buf)? {
            last_data = Instant::now();
            leftover = checker.feed(&buf[..n]);
        }
    }
    let window = last_data.duration_since(t0);
    checker.finish();
    peer.restore_pending(leftover);
    Ok(SourceResult {
        announced_bytes,
        frame_len,
        bytes: checker.fed,
        window,
        stats: checker.stats,
        complete: checker.done,
    })
}

/// Send frames for `send_for` (nothing when the phase is only being passed through), then wait
/// for the target to finish its `announced` window.
pub fn sink<L: Read + Write>(
    peer: &mut Peer<'_, L>,
    announced: Duration,
    send_for: Duration,
) -> Result<SinkResult, BenchError> {
    let t0 = Instant::now();
    let mut batch = vec![0; BATCH_FRAMES * FRAME_LEN];
    let (mut seq, mut stalls) = (0u32, 0u32);
    while t0.elapsed() < send_for {
        for (i, frame) in (0..).zip(batch.as_chunks_mut::<FRAME_LEN>().0) {
            encode_frame(seq.wrapping_add(i), frame);
        }
        peer.send_all(&batch, &mut stalls, t0 + send_for + SEND_SLACK)?;
        seq = seq.wrapping_add(BATCH_FRAMES as u32);
    }
    let elapsed = t0.elapsed();
    peer.await_marker(Marker::SinkDone, t0 + announced + DONE_SLACK)?;
    Ok(SinkResult { frames_sent: seq, bytes: u64::from(seq) * FRAME_LEN as u64, elapsed, stalls })
}

/// A probe of `size` bytes (at least 4) that carries its own sequence number.
pub fn build_probe(seq: u32, size: usize) -> Vec<u8> {
    let mut probe: Vec<u8> = (0..size).map(|i| (i as u8).wrapping_mul(31) ^ seq as u8).collect();
    probe[..4].copy_from_slice(&seq.to_le_bytes());
    probe
}

enum Reply {
    Matched,
    Corrupt,
}

/// Look through the received bytes for the reply to probe `seq`.
fn classify(got: &mut Vec<u8>, probe: &[u8], seq: u32, result: &mut EchoResult) -> Option<Reply> {
    while got.len() >= probe.len() {
        let reply: Vec<u8> = got.drain(..probe.len()).collect();
        let reply_seq = u32::from_le_bytes([reply[0], reply[1], reply[2], reply[3]]);
        match reply_seq.cmp(&seq) {
            std::cmp::Ordering::Equal => return Some(if reply == probe { Reply::Matched } else { Reply::Corrupt }),
            std::cmp::Ordering::Less => result.stale += 1,
            std::cmp::Ordering::Greater => {
                result.corrupt += 1;
                got.clear();
            },
        }
    }
    None
}

/// Send one probe at a time, time each reply, and wait for the target's end marker.
pub fn echo<L: Read + Write>(
    peer: &mut Peer<'_, L>,
    announced: Duration,
    send_for: Duration,
    size: u16,
    probe_timeout: Duration,
) -> Result<EchoResult, BenchError> {
    let size = usize::from(size);
    if size < 4 {
        return Err(BenchError::Unsupported { what: format!("echo probes of {size} bytes, the minimum is 4") });
    }
    peer.take_pending();
    let mut result = EchoResult { size, ..EchoResult::default() };
    let mut got = Vec::new();
    let mut buf = [0; 4096];
    let t0 = Instant::now();
    let mut seq = 0u32;
    while t0.elapsed() < send_for {
        peer.check_stop()?;
        let probe = build_probe(seq, size);
        let sent_at = Instant::now();
        peer.send_all(&probe, &mut 0, sent_at + probe_timeout)?;
        result.sent += 1;
        let verdict = loop {
            if let Some(verdict) = classify(&mut got, &probe, seq, &mut result) {
                break Some(verdict);
            }
            if sent_at.elapsed() >= probe_timeout {
                break None;
            }
            if let Some(n) = peer.read_into(&mut buf)? {
                got.extend_from_slice(&buf[..n]);
            }
        };
        match verdict {
            Some(Reply::Matched) => {
                result.received += 1;
                if result.rtts.len() < MAX_RTT_SAMPLES {
                    result.rtts.push(sent_at.elapsed());
                }
            },
            Some(Reply::Corrupt) => result.corrupt += 1,
            None => {
                result.lost += 1;
                got.clear();
            },
        }
        seq = seq.wrapping_add(1);
    }
    result.window = t0.elapsed();
    result.rtts.sort_unstable();
    peer.await_marker(Marker::EchoDone, t0 + announced + DONE_SLACK)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use assert2::{assert, check};
    use ferrite_wire::bench::{HEADER_LEN, source_stream};
    use rstest::rstest;

    use super::*;
    use crate::emit::Emitter;
    use crate::testing::FakeLink;

    fn run_source(mut stream: Vec<u8>, chunk: usize) -> (SourceChecker, Vec<u8>) {
        stream.extend_from_slice(b"BENCH source done\nrest");
        let mut checker = SourceChecker::new();
        let mut rest = Vec::new();
        for part in stream.chunks(chunk) {
            rest = checker.feed(part);
            if checker.done() {
                break;
            }
        }
        (checker, rest)
    }

    #[rstest]
    #[case::byte_at_a_time(1)]
    #[case::odd(13)]
    #[case::packet(1024)]
    #[case::whole(1 << 20)]
    fn a_clean_stream_checks_out_however_it_is_cut(#[case] chunk: usize) {
        let (checker, rest) = run_source(source_stream(8), chunk);
        check!(checker.done());
        check!((checker.stats.frames_ok, checker.stats.crc_failures, checker.stats.gaps) == (8, 0, 0));
        check!(checker.stats.resync_bytes == 0, "the end marker is not garbage");
        check!(checker.fed == 8 * FRAME_LEN as u64);
        check!(rest.len() <= b"\nrest".len());
    }

    #[test]
    fn the_bytes_after_the_marker_are_handed_back() {
        let (_, rest) = run_source(source_stream(2), 1 << 20);
        check!(rest == b"\nrest");
    }

    #[test]
    fn corruption_gaps_and_garbage_are_each_detected() {
        let mut stream = vec![9, 9];
        stream.extend_from_slice(&source_stream(6));
        stream[2 + FRAME_LEN + HEADER_LEN + 3] ^= 0xff;
        let lost = 2 + 3 * FRAME_LEN;
        stream.drain(lost..lost + FRAME_LEN);
        let (checker, _) = run_source(stream, 700);
        check!(checker.done());
        check!(checker.stats.frames_ok == 4);
        check!(checker.stats.crc_failures == 1);
        check!((checker.stats.gaps, checker.stats.missing) == (1, 1));
        check!(checker.stats.resync_bytes == 2);
    }

    #[test]
    fn a_stream_without_the_marker_is_not_done_but_loses_no_bytes() {
        let mut checker = SourceChecker::new();
        check!(checker.feed(&source_stream(2)).is_empty());
        checker.finish();
        check!(!checker.done());
        check!(checker.stats.frames_ok == 2);
    }

    fn peer_over<'a>(link: &'a mut FakeLink, emitter: &'a mut Emitter, stop: &'a AtomicBool) -> Peer<'a, FakeLink> {
        Peer::new(link, emitter, stop)
    }

    #[test]
    fn the_source_phase_reports_a_faulty_stream() {
        let mut stream = source_stream(4);
        stream[FRAME_LEN + HEADER_LEN + 1] ^= 1;
        stream.extend_from_slice(b"BENCH source done\n");
        let mut link = FakeLink::new().with_rx(&stream).max_read(900);
        let (mut emitter, stop) = (Emitter::new(), AtomicBool::new(false));
        let mut peer = peer_over(&mut link, &mut emitter, &stop);
        assert!(let Ok(result) = source(&mut peer, 4 * FRAME_LEN as u64, FRAME_LEN as u16, Duration::from_secs(5), Duration::from_secs(5)));
        check!(result.complete);
        check!(!result.healthy());
        check!((result.stats.frames_ok, result.stats.crc_failures) == (3, 1));
        check!(result.bytes == 4 * FRAME_LEN as u64);
    }

    #[test]
    fn a_stalled_source_stream_ends_incomplete() {
        let mut link = FakeLink::new().with_rx(&source_stream(1));
        let (mut emitter, stop) = (Emitter::new(), AtomicBool::new(false));
        let mut peer = peer_over(&mut link, &mut emitter, &stop);
        assert!(let Ok(result) = source(&mut peer, 2 * FRAME_LEN as u64, FRAME_LEN as u16, Duration::from_millis(30), Duration::from_secs(5)));
        check!(!result.complete);
        check!(result.stats.frames_ok == 1);
        check!(!result.healthy());
    }

    #[test]
    fn the_sink_phase_sends_a_verifiable_frame_sequence() {
        let mut link = FakeLink::new().with_rx(b"BENCH sink done\n");
        let (mut emitter, stop) = (Emitter::new(), AtomicBool::new(false));
        let mut peer = peer_over(&mut link, &mut emitter, &stop);
        assert!(let Ok(result) = sink(&mut peer, Duration::ZERO, Duration::from_millis(20)));
        check!(result.frames_sent > 0);
        check!(link.written.len() as u64 == result.bytes);

        let mut reader = FrameReader::new();
        let mut stats = SinkStats::default();
        reader.feed(&link.written, |event| stats.observe(event));
        check!((stats.crc_failures, stats.gaps, stats.resync_bytes) == (0, 0, 0));
        check!(stats.frames_ok == result.frames_sent);
    }

    #[test]
    fn a_passed_through_sink_sends_nothing_but_still_waits_for_the_end() {
        let mut link = FakeLink::new().with_rx(b"noise\nBENCH sink done\n");
        let (mut emitter, stop) = (Emitter::new(), AtomicBool::new(false));
        let mut peer = peer_over(&mut link, &mut emitter, &stop);
        assert!(let Ok(result) = sink(&mut peer, Duration::ZERO, Duration::ZERO));
        check!(result.frames_sent == 0);
        check!(link.written.is_empty());
    }

    #[test]
    fn echo_times_every_reply_and_sorts_the_samples() {
        let mut link = FakeLink::new()
            .responding(|written| vec![(Duration::ZERO, written.to_vec())])
            .with_rx_after(Duration::from_millis(60), b"BENCH echo done\n");
        let (mut emitter, stop) = (Emitter::new(), AtomicBool::new(false));
        let mut peer = peer_over(&mut link, &mut emitter, &stop);
        assert!(let Ok(result) = echo(&mut peer, Duration::ZERO, Duration::from_millis(30), 64, Duration::from_secs(1)));
        check!(result.received > 0);
        check!((result.lost, result.corrupt, result.stale) == (0, 0, 0));
        check!(usize::try_from(result.received).ok() == Some(result.rtts.len()));
        check!(result.rtts.is_sorted());
    }

    #[test]
    fn echo_counts_a_lost_probe_and_recovers_for_the_next() {
        let mut dropped = false;
        let mut link = FakeLink::new().with_rx_after(Duration::from_millis(250), b"BENCH echo done\n").responding(
            move |written| {
                if dropped || written[0] != 1 {
                    return vec![(Duration::ZERO, written.to_vec())];
                }
                dropped = true;
                Vec::new()
            },
        );
        let (mut emitter, stop) = (Emitter::new(), AtomicBool::new(false));
        let mut peer = peer_over(&mut link, &mut emitter, &stop);
        assert!(let Ok(result) = echo(&mut peer, Duration::ZERO, Duration::from_millis(150), 64, Duration::from_millis(20)));
        check!(result.lost == 1);
        check!(result.received >= 2);
    }

    #[test]
    fn echo_rejects_a_corrupted_reply() {
        let mut link =
            FakeLink::new().with_rx_after(Duration::from_millis(60), b"BENCH echo done\n").responding(|written| {
                let mut reply = written.to_vec();
                reply[10] ^= 0xff;
                vec![(Duration::ZERO, reply)]
            });
        let (mut emitter, stop) = (Emitter::new(), AtomicBool::new(false));
        let mut peer = peer_over(&mut link, &mut emitter, &stop);
        assert!(let Ok(result) = echo(&mut peer, Duration::ZERO, Duration::from_millis(20), 64, Duration::from_secs(1)));
        check!(result.corrupt > 0);
        check!(result.received == 0);
    }

    #[test]
    fn probes_are_distinct_and_carry_their_sequence() {
        let (a, b) = (build_probe(1, 64), build_probe(2, 64));
        check!(a.len() == 64);
        check!(a[..4] == 1u32.to_le_bytes());
        check!(a != b);
        check!(build_probe(7, 4) == 7u32.to_le_bytes());
    }
}
