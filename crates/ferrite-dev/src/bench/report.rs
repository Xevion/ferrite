//! What each phase measured on the desktop, next to the target's own summary lines.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use ferrite_wire::bench::{SinkStats, percentile};
use strum::VariantArray as _;

use super::Phase;

/// Megabytes (10^6 bytes) per second, formatted like the target's own rate.
pub struct Rate {
    pub bytes: u64,
    pub over: Duration,
}

impl fmt::Display for Rate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let micros = self.over.as_micros();
        if micros == 0 {
            return f.write_str("-");
        }
        let milli = u128::from(self.bytes) * 1000 / micros;
        write!(f, "{}.{:03}MB/s", milli / 1000, milli % 1000)
    }
}

#[derive(Debug)]
pub struct SourceResult {
    pub announced_bytes: u64,
    pub frame_len: u16,
    pub bytes: u64,
    /// From the marker to the last byte before the end marker.
    pub window: Duration,
    pub stats: SinkStats,
    /// Whether the end marker arrived.
    pub complete: bool,
}

impl SourceResult {
    pub fn expected_frames(&self) -> u64 {
        self.announced_bytes / u64::from(self.frame_len.max(1))
    }

    pub fn healthy(&self) -> bool {
        self.complete
            && u64::from(self.stats.frames_ok) == self.expected_frames()
            && self.stats.crc_failures == 0
            && self.stats.gaps == 0
            && self.stats.resync_bytes == 0
    }
}

#[derive(Debug)]
pub struct SinkResult {
    pub frames_sent: u32,
    pub bytes: u64,
    pub elapsed: Duration,
    /// Writes that found the link full and had to wait.
    pub stalls: u32,
}

#[derive(Debug, Default)]
pub struct EchoResult {
    pub size: usize,
    pub sent: u32,
    pub received: u32,
    pub lost: u32,
    pub corrupt: u32,
    /// Replies to probes already given up on.
    pub stale: u32,
    pub window: Duration,
    /// Round-trip times, ascending.
    pub rtts: Vec<Duration>,
}

impl EchoResult {
    pub fn rtt(&self, pct: usize) -> Duration {
        percentile(&self.rtts, pct).unwrap_or_default()
    }
}

#[derive(Debug, Default)]
pub struct Report {
    /// Whether the target announced its first phase.
    pub started: bool,
    /// The transport the last session ran over, with its parameters.
    pub transport: Option<String>,
    pub source: Option<SourceResult>,
    pub sink: Option<SinkResult>,
    pub echo: Option<EchoResult>,
    pub target: BTreeMap<Phase, String>,
    pub aborted: Option<String>,
}

impl Report {
    pub fn render(&self) -> Vec<String> {
        let mut out = vec!["--- bench summary ---".to_owned()];
        out.extend(self.transport.as_ref().map(|transport| format!("transport: {transport}")));
        if let Some(s) = &self.source {
            out.push(row(Phase::Source, "desktop", &source_line(s)));
        }
        out.extend(self.target_row(Phase::Source));
        if let Some(s) = &self.sink {
            out.push(row(Phase::Sink, "desktop", &sink_line(s)));
        }
        out.extend(self.target_row(Phase::Sink));
        if let Some(e) = &self.echo {
            out.push(row(Phase::Echo, "desktop", &echo_line(e)));
        }
        out.extend(self.target_row(Phase::Echo));
        out.extend(self.target_row(Phase::Micro));
        if let Some(reason) = &self.aborted {
            out.push(format!("INCOMPLETE: {reason}"));
        }
        out
    }

    fn target_row(&self, phase: Phase) -> Option<String> {
        self.target.get(&phase).map(|line| row(phase, "target ", line))
    }
}

fn row(phase: Phase, side: &str, text: &str) -> String {
    format!("{phase:<6} {side}: {text}")
}

fn source_line(s: &SourceResult) -> String {
    let verdict = if s.healthy() { "ok" } else { "FAULTS" };
    format!(
        "{verdict} bytes={} window={:.3}s rate={} frames_ok={}/{} crc_failures={} seq_gaps={} seq_missing={} resync_bytes={}{}",
        s.bytes,
        s.window.as_secs_f64(),
        Rate { bytes: s.bytes, over: s.window },
        s.stats.frames_ok,
        s.expected_frames(),
        s.stats.crc_failures,
        s.stats.gaps,
        s.stats.missing,
        s.stats.resync_bytes,
        if s.complete { "" } else { " no end marker" },
    )
}

fn sink_line(s: &SinkResult) -> String {
    format!(
        "sent frames={} bytes={} elapsed={:.3}s rate={} write_stalls={}",
        s.frames_sent,
        s.bytes,
        s.elapsed.as_secs_f64(),
        Rate { bytes: s.bytes, over: s.elapsed },
        s.stalls,
    )
}

fn echo_line(e: &EchoResult) -> String {
    let us = |d: Duration| d.as_micros();
    format!(
        "size={} samples={} sent={} lost={} corrupt={} stale={} rtt_us p50={} p99={} max={}",
        e.size,
        e.rtts.len(),
        e.sent,
        e.lost,
        e.corrupt,
        e.stale,
        us(e.rtt(50)),
        us(e.rtt(99)),
        us(e.rtt(100)),
    )
}

/// The phase and text of a target summary line such as `bench echo: chunks=...`, wherever in the
/// line the logger put it.
pub fn target_summary(line: &str) -> Option<(Phase, &str)> {
    Phase::VARIANTS.iter().copied().find_map(|phase| {
        let key = format!("bench {}:", <&str>::from(phase));
        let at = line.find(&key)?;
        Some((phase, line[at + key.len()..].trim()))
    })
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};
    use ferrite_wire::bench::FrameEvent;
    use rstest::rstest;

    use super::*;

    fn stats_with(valid_frames: u32) -> SinkStats {
        let mut stats = SinkStats::default();
        (0..valid_frames).for_each(|seq| stats.observe(FrameEvent::Valid { seq }));
        stats
    }

    #[rstest]
    #[case::bare("bench source: bytes=4194304 frames=4096", Some((Phase::Source, "bytes=4194304 frames=4096")))]
    #[case::behind_a_log_prefix("[  12.345 INFO ] bench micro: per_read_ns=900", Some((Phase::Micro, "per_read_ns=900")))]
    #[case::not_run("bench sink: not run", Some((Phase::Sink, "not run")))]
    #[case::marker_line("BENCH sink 10", None)]
    #[case::other("bench: something", None)]
    fn target_summary_lines_are_recognised(#[case] line: &str, #[case] want: Option<(Phase, &str)>) {
        check!(target_summary(line) == want);
    }

    #[test]
    fn rates_match_the_targets_format() {
        check!(Rate { bytes: 4_194_304, over: Duration::from_millis(1000) }.to_string() == "4.194MB/s");
        check!(Rate { bytes: 1, over: Duration::ZERO }.to_string() == "-");
    }

    #[test]
    fn echo_percentiles_come_from_the_sorted_samples() {
        let echo = EchoResult { rtts: (1..=100).map(Duration::from_micros).collect(), ..EchoResult::default() };
        check!(echo.rtt(50) == Duration::from_micros(50));
        check!(echo.rtt(99) == Duration::from_micros(99));
        check!(echo.rtt(100) == Duration::from_micros(100));
        check!(EchoResult::default().rtt(50) == Duration::ZERO);
    }

    #[test]
    fn the_summary_puts_each_target_line_under_its_desktop_line() {
        let mut report = Report {
            source: Some(SourceResult {
                announced_bytes: 2048,
                frame_len: 1024,
                bytes: 2048,
                window: Duration::from_secs(1),
                stats: stats_with(2),
                complete: true,
            }),
            ..Report::default()
        };
        report.target.insert(Phase::Source, "bytes=2048 rate=2.048MB/s".into());
        report.target.insert(Phase::Micro, "per_read_ns=900".into());
        let lines = report.render();
        assert!(lines.len() == 4, "{lines:?}");
        check!(lines[1].starts_with("source desktop: ok bytes=2048 window=1.000s rate=0.002MB/s frames_ok=2/2"));
        check!(lines[2] == "source target : bytes=2048 rate=2.048MB/s");
        check!(lines[3] == "micro  target : per_read_ns=900");
    }

    #[test]
    fn the_summary_names_the_transport_it_ran_over() {
        let report = Report { transport: Some("usb (bus 3 address 12)".into()), ..Report::default() };
        check!(report.render() == ["--- bench summary ---", "transport: usb (bus 3 address 12)"]);
    }

    #[test]
    fn a_missing_frame_or_marker_is_a_fault() {
        let mut source = SourceResult {
            announced_bytes: 2048,
            frame_len: 1024,
            bytes: 1024,
            window: Duration::from_secs(1),
            stats: stats_with(1),
            complete: true,
        };
        check!(!source.healthy());
        source.stats = stats_with(2);
        check!(source.healthy());
        source.complete = false;
        check!(!source.healthy());
    }
}
