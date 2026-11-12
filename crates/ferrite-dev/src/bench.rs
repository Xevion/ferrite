//! `bench`: the desktop peer for the `dbcbench` app.
//!
//! The target announces each phase only after the desktop sends the ready line, and the phases
//! always come in the same order. A phase left out of `--phases` is still followed, so the
//! target can advance, but nothing is sent for it and nothing is reported.

mod peer;
mod phases;
mod report;

use std::io::{self, Read, Write};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use clap::ValueEnum;
use ferrite_wire::bench::Marker;
use snafu::{ResultExt, Snafu};

use self::peer::Peer;
pub use self::report::Report;
use crate::config::Config;
use crate::connect::{ConnectError, Connector, Waited};
use crate::emit::Emitter;

/// How long the target gets to announce each phase after the first.
const PHASE_WAIT: Duration = Duration::from_secs(30);
const MICRO_WAIT: Duration = Duration::from_secs(30);
const SUMMARY_WAIT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub enum Phase {
    Source,
    Sink,
    Echo,
    Micro,
}

impl Phase {
    pub const ALL: [Self; 4] = [Self::Source, Self::Sink, Self::Echo, Self::Micro];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Sink => "sink",
            Self::Echo => "echo",
            Self::Micro => "micro",
        }
    }
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(self.name())
    }
}

#[derive(Debug, Snafu)]
pub enum BenchError {
    #[snafu(display("cannot connect to the DbC link"))]
    Connect { source: ConnectError },

    #[snafu(display("the link to the target failed"))]
    Io { source: io::Error },

    #[snafu(display("the target closed the link"))]
    Closed,

    #[snafu(display("timed out waiting for {waiting_for}"))]
    Timeout { waiting_for: String },

    #[snafu(display("expected `{expected}` from the target but it sent `{got}`"))]
    Unexpected { expected: String, got: String },

    #[snafu(display("not supported: {what}"))]
    Unsupported { what: String },

    #[snafu(display("stopped before the benchmark finished"))]
    Stopped,
}

impl BenchError {
    const fn is_link_loss(&self) -> bool {
        matches!(self, Self::Io { .. } | Self::Closed)
    }
}

#[derive(Debug, Clone)]
pub struct BenchOptions {
    pub phases: Vec<Phase>,
    /// How long to wait for the device and the target's first announcement.
    pub wait: Duration,
}

/// Wait for the target, run the phases, and print the summary next to the target's own lines.
pub fn run(
    config: &Config,
    options: &BenchOptions,
    emitter: &mut Emitter,
    stop: &AtomicBool,
) -> Result<(), BenchError> {
    let deadline = Instant::now() + options.wait;
    emitter.line(&format!(
        "--- bench start, waiting up to {} for the target ---",
        humantime::format_duration(options.wait)
    ));
    let mut connector = Connector::new(config);
    let mut report = Report::default();
    let outcome = loop {
        match connector.wait(emitter, Some(deadline), stop).context(ConnectSnafu)? {
            Waited::Connected(mut tty) => {
                let result = session(&mut Peer::new(&mut tty, emitter, stop), &options.phases, deadline, &mut report);
                match result {
                    Err(e) if e.is_link_loss() => {
                        emitter.line("--- link down ---");
                        connector.link_lost();
                        if report.started {
                            break Err(e);
                        }
                    },
                    other => break other,
                }
            },
            Waited::TimedOut => {
                break Err(BenchError::Timeout { waiting_for: "the DbC device and the target's first phase".into() });
            },
            Waited::Stopped => break Err(BenchError::Stopped),
        }
    };
    if report.started {
        if let Err(e) = &outcome {
            report.aborted = Some(crate::chain(e));
        }
        report.render().iter().for_each(|line| emitter.plain(line));
    }
    outcome
}

/// Play one connection's worth of phases. Whatever was measured before a failure stays in `report`.
fn session<L: Read + Write>(
    peer: &mut Peer<'_, L>,
    selected: &[Phase],
    first_until: Instant,
    report: &mut Report,
) -> Result<(), BenchError> {
    let outcome = play(peer, selected, first_until, report);
    report
        .target
        .extend(peer.summaries.iter().filter(|(phase, _)| selected.contains(phase)).map(|(p, s)| (*p, s.clone())));
    outcome
}

fn play<L: Read + Write>(
    peer: &mut Peer<'_, L>,
    selected: &[Phase],
    first_until: Instant,
    report: &mut Report,
) -> Result<(), BenchError> {
    let wants = |phase| selected.contains(&phase);

    let marker = peer.begin("the source phase", first_until)?;
    report.started = true;
    let Marker::Source { bytes, frame_len } = marker else {
        return Err(unexpected("BENCH source <bytes> <frame_len>", marker));
    };
    peer.event(&format!("phase source: {bytes} bytes in {frame_len}-byte frames"));
    let source = phases::source(peer, bytes, frame_len, phases::STREAM_IDLE, phases::SOURCE_LIMIT)?;
    let complete = source.complete;
    if wants(Phase::Source) {
        report.source = Some(source);
    }
    if !complete {
        return Err(BenchError::Timeout { waiting_for: "the end of the source stream".into() });
    }

    let marker = peer.begin("the sink phase", Instant::now() + PHASE_WAIT)?;
    let Marker::Sink { secs } = marker else { return Err(unexpected("BENCH sink <secs>", marker)) };
    let announced = Duration::from_secs(secs.into());
    peer.event(&format!("phase sink: {secs} s"));
    let sink = phases::sink(peer, announced, if wants(Phase::Sink) { announced } else { Duration::ZERO })?;
    if wants(Phase::Sink) {
        report.sink = Some(sink);
    }

    let marker = peer.begin("the echo phase", Instant::now() + PHASE_WAIT)?;
    let Marker::Echo { secs, size } = marker else { return Err(unexpected("BENCH echo <secs> <size>", marker)) };
    let announced = Duration::from_secs(secs.into());
    peer.event(&format!("phase echo: {secs} s, {size}-byte probes"));
    let send_for = if wants(Phase::Echo) { announced.saturating_sub(phases::ECHO_MARGIN) } else { Duration::ZERO };
    let echo = phases::echo(peer, announced, send_for, size, phases::PROBE_TIMEOUT)?;
    if wants(Phase::Echo) {
        report.echo = Some(echo);
    }

    let marker = peer.begin("the micro phase", Instant::now() + PHASE_WAIT)?;
    if marker != Marker::Micro {
        return Err(unexpected("BENCH micro", marker));
    }
    peer.await_marker(Marker::Done, Instant::now() + MICRO_WAIT)?;
    peer.collect_summaries(&Phase::ALL, Instant::now() + SUMMARY_WAIT)
}

fn unexpected(expected: &str, got: Marker) -> BenchError {
    BenchError::Unexpected { expected: expected.into(), got: got.to_string() }
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};
    use ferrite_wire::bench::{FRAME_LEN, source_stream};

    use super::*;
    use crate::emit::tests::Shared;
    use crate::testing::{FakeLink, Replies};

    fn ready_count(written: &[u8]) -> usize {
        String::from_utf8_lossy(written).matches("BENCH ready\n").count()
    }

    /// A target that answers each ready line with the next phase, the way dbcbench does.
    fn target() -> impl FnMut(&[u8]) -> Replies {
        let mut phase = 0;
        move |written| {
            if ready_count(written) == 0 {
                return Vec::new();
            }
            phase += 1;
            let now = |bytes: &[u8]| (Duration::ZERO, bytes.to_vec());
            let soon = |bytes: &[u8]| (Duration::from_millis(30), bytes.to_vec());
            match phase {
                1 => {
                    let mut out = b"BENCH source 2048 1024\n".to_vec();
                    out.extend(source_stream(2));
                    out.extend(b"BENCH source done\n");
                    vec![now(&out)]
                },
                2 => vec![now(b"BENCH sink 0\n"), soon(b"BENCH sink done\n")],
                3 => vec![now(b"BENCH echo 0 64\n"), soon(b"BENCH echo done\n")],
                _ => vec![
                    now(b"BENCH micro\nBENCH done\n"),
                    now(b"[ 1.0 INFO ] bench source: bytes=2048 rate=1.000MB/s\nbench sink: not run\n"),
                    now(b"bench echo: chunks=0\nbench micro: per_read_ns=900\n"),
                ],
            }
        }
    }

    #[test]
    fn a_full_session_is_ready_gated_and_pairs_desktop_and_target_numbers() {
        let mut link = FakeLink::new().max_read(333).responding(target());
        let out = Shared::default();
        let mut emitter = Emitter::new().with_sink(out.clone()).with_stamp(|| "T".into());
        let stop = AtomicBool::new(false);
        let mut report = Report::default();
        let outcome = session(
            &mut Peer::new(&mut link, &mut emitter, &stop),
            &Phase::ALL,
            Instant::now() + Duration::from_secs(5),
            &mut report,
        );
        assert!(let Ok(()) = outcome);

        check!(link.written == "BENCH ready\n".repeat(4).as_bytes(), "one ready line before each phase");
        assert!(let Some(source) = &report.source);
        check!(source.healthy());
        check!(source.bytes == 2 * FRAME_LEN as u64);
        check!(report.sink.as_ref().is_some_and(|s| s.frames_sent == 0));
        check!(report.echo.as_ref().is_some_and(|e| e.sent == 0));
        check!(report.target.len() == 4);
        check!(report.target[&Phase::Source] == "bytes=2048 rate=1.000MB/s");

        let lines = report.render();
        check!(lines.iter().any(|l| l == "micro  target : per_read_ns=900"), "{lines:?}");
        check!(out.text().contains("T target: bench sink: not run\n"));
    }

    #[test]
    fn phases_left_out_are_followed_but_not_exercised_or_reported() {
        let mut link = FakeLink::new().responding(target());
        let mut emitter = Emitter::new();
        let stop = AtomicBool::new(false);
        let mut report = Report::default();
        let selected = [Phase::Source, Phase::Micro];
        let outcome = session(
            &mut Peer::new(&mut link, &mut emitter, &stop),
            &selected,
            Instant::now() + Duration::from_secs(5),
            &mut report,
        );
        assert!(let Ok(()) = outcome);
        check!(ready_count(&link.written) == 4, "the target still needs a ready line for each phase");
        check!(report.sink.is_none() && report.echo.is_none());
        check!(report.target.keys().copied().collect::<Vec<_>>() == [Phase::Source, Phase::Micro]);
    }

    #[test]
    fn a_target_that_never_answers_times_out_and_keeps_resending_ready() {
        let mut link = FakeLink::new();
        let mut emitter = Emitter::new();
        let stop = AtomicBool::new(false);
        let mut report = Report::default();
        let mut peer = Peer::new(&mut link, &mut emitter, &stop).resend_ready_every(Duration::from_millis(20));
        let outcome = session(&mut peer, &Phase::ALL, Instant::now() + Duration::from_millis(100), &mut report);
        assert!(let Err(BenchError::Timeout { .. }) = outcome);
        check!(!report.started);
        check!(ready_count(&link.written) >= 3);
    }

    #[test]
    fn a_marker_out_of_order_is_a_protocol_error() {
        let mut link = FakeLink::new().with_rx(b"BENCH echo 10 64\n");
        let mut emitter = Emitter::new();
        let stop = AtomicBool::new(false);
        let mut report = Report::default();
        let outcome = session(
            &mut Peer::new(&mut link, &mut emitter, &stop),
            &Phase::ALL,
            Instant::now() + Duration::from_secs(1),
            &mut report,
        );
        assert!(let Err(BenchError::Unexpected { got, .. }) = outcome);
        check!(got == "BENCH echo 10 64");
    }

    #[test]
    fn a_closed_link_mid_run_is_a_link_loss_with_the_partial_report_kept() {
        let mut stream = b"BENCH source 4096 1024\n".to_vec();
        stream.extend(source_stream(1));
        let mut link = FakeLink::new().with_rx(&stream).closing();
        let mut emitter = Emitter::new();
        let stop = AtomicBool::new(false);
        let mut report = Report::default();
        let outcome = session(
            &mut Peer::new(&mut link, &mut emitter, &stop),
            &Phase::ALL,
            Instant::now() + Duration::from_secs(1),
            &mut report,
        );
        assert!(let Err(e) = outcome);
        check!(e.is_link_loss());
        check!(report.started);
    }
}
