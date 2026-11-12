//! `dbcbench`: measure what the DbC link really does, so tuning is guided by numbers.
//!
//! After the same bring-up as `dbcping` it runs four phases against a desktop peer, each
//! announced by a marker line once the desktop says `BENCH ready`:
//!
//!   - source: send 4 MiB as 1 KiB checksummed frames and time it to the last ack;
//!   - sink: receive and verify frames from the desktop for 10 s;
//!   - echo: reply to each 64-byte message at once for 10 s, the desktop timing round trips;
//!   - micro: the cost of a DCCTRL read through the root bridge, a log line and a log flush.
//!
//! The whole run receives on 16 KiB IN TRBs, which complete only when full or on a short packet,
//! so the desktop peer must end every write that is a multiple of 1 KiB with a zero-length packet. `ferrite-dev bench`
//! over direct usbfs does; the `usb_serial` tty never sends one and can no longer drive this tool.
//!
//! Measured regions log nothing and only record timestamps and counters in memory. The
//! results are logged afterwards to the console and `\EFI\ferrite\dbcbench.txt`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::vec::Vec;
use core::hint::black_box;
use core::time::Duration;

use ferrite_efi::bootlog::{BootLog, Echo};
use ferrite_efi::clock::Clock;
use ferrite_efi::pci::RootBridge;
use ferrite_efi::xhci::{self, OpenDbc};
use ferrite_efi::{logger, session};
use ferrite_hal::{Clock as _, Instant, Mmio};
use ferrite_recon::bringup::{self, Link};
use ferrite_recon::display::{Chain, Ms, Rate};
use ferrite_recon::summary;
use ferrite_wire::bench::{self, FRAME_LEN, FrameReader, FrameSource, Marker, ReadyDetector, SinkStats};
use ferrite_xhci::dbc::OUT_BYTES;
use ferrite_xhci::{Config, InTrbSize, State};
use log::LevelFilter;
use uefi::prelude::*;

const EXIT_AFTER: Duration = Duration::from_secs(10);

/// 512 MiB: about 10 s at the tty path's rate, long enough to settle the number.
const SOURCE_FRAMES: u32 = 1 << 19;
const SOURCE_TIMEOUT: Duration = Duration::from_secs(60);
const SINK_WINDOW: Duration = Duration::from_secs(10);
const ECHO_WINDOW: Duration = Duration::from_secs(10);
const ECHO_MESSAGE: usize = 64;
/// Round-trip handling times kept in memory; later ones are only counted.
const ECHO_SAMPLES: usize = 1 << 16;
const MICRO_READS: u32 = 1000;

/// How long the desktop gets to start the tool before the first phase.
const FIRST_READY_TIMEOUT: Duration = Duration::from_secs(120);
const READY_TIMEOUT: Duration = Duration::from_secs(60);
const MARKER_TIMEOUT: Duration = Duration::from_secs(5);

/// Poll calls and the time spent inside them.
#[derive(Clone, Copy, Debug, Default)]
struct Polled {
    polls: u64,
    inside: Duration,
}

impl Polled {
    fn poll(&mut self, dbc: &mut OpenDbc, clock: Clock, rx: impl FnMut(&[u8])) {
        let t0 = clock.now();
        dbc.poll(rx);
        self.inside += clock.now() - t0;
        self.polls += 1;
    }
}

struct SourceResult {
    bytes: u64,
    frames: u32,
    /// Until the last frame was queued.
    queued_in: Duration,
    /// Until the last byte was acknowledged.
    elapsed: Duration,
    refused_frames: u32,
    polled: Polled,
    complete: bool,
}

struct SinkResult {
    bytes: u64,
    /// From the first received byte to the last.
    window: Duration,
    stats: SinkStats,
    polled: Polled,
    complete: bool,
}

struct EchoResult {
    chunks: u32,
    bytes: u64,
    refused_bytes: usize,
    /// Per received chunk: from the poll that delivered it to its reply being queued.
    turnaround: Vec<Duration>,
    unsampled: u32,
    polled: Polled,
    complete: bool,
}

struct MicroResult {
    read_total: Duration,
    direct_total: Duration,
    poll_total: Duration,
    line: Duration,
    flush: Duration,
    flushed: bool,
}

#[derive(Default)]
struct Results {
    source: Option<SourceResult>,
    sink: Option<SinkResult>,
    echo: Option<EchoResult>,
    micro: Option<MicroResult>,
}

#[entry]
fn main() -> Status {
    let helpers = uefi::helpers::init();
    logger::init(BootLog::open("dbcbench", Echo::Console), LevelFilter::Debug);
    if let Err(e) = helpers {
        log::warn!("uefi helpers init failed: {e}");
    }
    let clock = logger::with(|log| log.clock()).unwrap_or_else(Clock::start);
    let start = clock.now();
    log::info!("ferrite-uefi :: dbcbench - DbC throughput and latency");

    let link =
        bringup::bring_up(clock, start, &Config { in_trb: InTrbSize::Kib16, ..Config::default() }).map(|mut link| {
            let results = run(&mut link, clock);
            log_results(&results);
            summary::report(&mut link.dbc, start);
            link
        });

    logger::with(|log| session::finish(log, EXIT_AFTER));
    drop(link);
    Status::SUCCESS
}

/// Finish the survey, then run the phases in order, stopping at the first one the desktop
/// does not answer.
fn run(link: &mut Link, clock: Clock) -> Results {
    let Link { dbc, survey } = link;
    survey.finish();
    let mut results = Results::default();
    let Some(source) = source(dbc, clock) else { return results };
    results.source = Some(source);
    let Some(sink) = sink(dbc, clock) else { return results };
    results.sink = Some(sink);
    let Some(echo) = echo(dbc, clock) else { return results };
    results.echo = Some(echo);
    results.micro = micro(dbc, clock);
    announce(dbc, clock, Marker::Done);
    results
}

fn alive(dbc: &OpenDbc) -> bool {
    !matches!(dbc.state(), State::Faulted | State::Disabled)
}

/// Poll until every queued byte is acknowledged.
fn drain(dbc: &mut OpenDbc, clock: Clock, timeout: Duration) -> bool {
    let t0 = clock.now();
    while dbc.buffered() > 0 {
        dbc.poll(|_| {});
        if !alive(dbc) || clock.now() - t0 >= timeout {
            return false;
        }
    }
    true
}

/// Send a marker line and wait until the desktop has taken it.
fn announce(dbc: &mut OpenDbc, clock: Clock, marker: Marker) -> bool {
    dbc.write(format!("{marker}\n").as_bytes()).is_complete() && drain(dbc, clock, MARKER_TIMEOUT)
}

/// Wait for the desktop's ready line.
fn wait_ready(dbc: &mut OpenDbc, clock: Clock, timeout: Duration) -> bool {
    let mut detector = ReadyDetector::default();
    let t0 = clock.now();
    loop {
        let mut ready = false;
        dbc.poll(|bytes| ready |= detector.feed(bytes));
        if ready {
            return true;
        }
        if !alive(dbc) || clock.now() - t0 >= timeout {
            log::error!("no ready line from the desktop within {} s", timeout.as_secs());
            return false;
        }
    }
}

/// Wait for the desktop, then announce a phase.
fn begin(dbc: &mut OpenDbc, clock: Clock, timeout: Duration, marker: Marker) -> bool {
    wait_ready(dbc, clock, timeout) && announce(dbc, clock, marker)
}

fn source(dbc: &mut OpenDbc, clock: Clock) -> Option<SourceResult> {
    let mut frames = FrameSource::new();
    // Whole frames that fit the free ring space go down in one write, so the driver cuts large TRBs.
    let mut stage: Vec<u8> = Vec::with_capacity(OUT_BYTES);
    let total = SOURCE_FRAMES as usize * FRAME_LEN;
    if !begin(dbc, clock, FIRST_READY_TIMEOUT, Marker::Source { bytes: total as u64, frame_len: FRAME_LEN as u16 }) {
        return None;
    }
    let mut polled = Polled::default();
    let (mut sent, mut refused_frames, mut blocked, mut complete) = (0, 0, false, true);
    let sent_before = dbc.stats().bytes_sent;
    let t0 = clock.now();
    while sent < total {
        let fit = ((OUT_BYTES - dbc.buffered()) / FRAME_LEN).min((total - sent) / FRAME_LEN);
        if fit > 0 {
            let first = sent / FRAME_LEN;
            stage.clear();
            (first..first + fit).for_each(|seq| stage.extend_from_slice(frames.frame(seq as u32)));
            sent += dbc.write(&stage).accepted;
            blocked = false;
        } else if !blocked {
            refused_frames += 1;
            blocked = true;
        }
        polled.poll(dbc, clock, |_| {});
        if !alive(dbc) || clock.now() - t0 >= SOURCE_TIMEOUT {
            complete = false;
            break;
        }
    }
    let queued_in = clock.now() - t0;
    while complete && dbc.buffered() > 0 {
        polled.poll(dbc, clock, |_| {});
        complete = alive(dbc) && clock.now() - t0 < SOURCE_TIMEOUT;
    }
    let elapsed = clock.now() - t0;
    let bytes = dbc.stats().bytes_sent - sent_before;
    announce(dbc, clock, Marker::SourceDone);
    Some(SourceResult { bytes, frames: SOURCE_FRAMES, queued_in, elapsed, refused_frames, polled, complete })
}

fn sink(dbc: &mut OpenDbc, clock: Clock) -> Option<SinkResult> {
    if !begin(dbc, clock, READY_TIMEOUT, Marker::Sink { secs: SINK_WINDOW.as_secs() as u32 }) {
        return None;
    }
    let mut reader = FrameReader::new();
    let mut stats = SinkStats::default();
    let mut polled = Polled::default();
    let mut first: Option<Instant> = None;
    let received_before = dbc.stats().bytes_received;
    let t0 = clock.now();
    let mut last = t0;
    let mut complete = true;
    loop {
        polled.poll(dbc, clock, |bytes| {
            last = clock.now();
            first.get_or_insert(last);
            reader.feed(bytes, |event| stats.observe(event));
        });
        if !alive(dbc) {
            complete = false;
            break;
        }
        if clock.now() - t0 >= SINK_WINDOW {
            break;
        }
    }
    let bytes = dbc.stats().bytes_received - received_before;
    let window = first.map_or(Duration::ZERO, |first| last - first);
    announce(dbc, clock, Marker::SinkDone);
    Some(SinkResult { bytes, window, stats, polled, complete })
}

fn echo(dbc: &mut OpenDbc, clock: Clock) -> Option<EchoResult> {
    if !begin(dbc, clock, READY_TIMEOUT, Marker::Echo { secs: ECHO_WINDOW.as_secs() as u32, size: ECHO_MESSAGE as u16 })
    {
        return None;
    }
    let mut turnaround = Vec::with_capacity(ECHO_SAMPLES);
    let mut got: Vec<u8> = Vec::with_capacity(8 * 1024);
    let mut polled = Polled::default();
    let (mut chunks, mut bytes, mut refused_bytes, mut unsampled, mut complete) = (0, 0u64, 0, 0, true);
    let t0 = clock.now();
    loop {
        got.clear();
        let mut seen = None;
        polled.poll(dbc, clock, |rx| {
            seen.get_or_insert_with(|| clock.now());
            got.extend_from_slice(rx);
        });
        if let Some(seen) = seen {
            chunks += 1;
            bytes += got.len() as u64;
            refused_bytes += dbc.write(&got).refused;
            if turnaround.len() < ECHO_SAMPLES {
                turnaround.push(clock.now() - seen);
            } else {
                unsampled += 1;
            }
        }
        if !alive(dbc) {
            complete = false;
            break;
        }
        if clock.now() - t0 >= ECHO_WINDOW {
            break;
        }
    }
    drain(dbc, clock, MARKER_TIMEOUT);
    announce(dbc, clock, Marker::EchoDone);
    turnaround.sort_unstable();
    Some(EchoResult { chunks, bytes, refused_bytes, turnaround, unsampled, polled, complete })
}

fn micro(dbc: &mut OpenDbc, clock: Clock) -> Option<MicroResult> {
    if !begin(dbc, clock, READY_TIMEOUT, Marker::Micro) {
        return None;
    }
    let ctrl = match xhci::find_xhci(dbc.device().addr()) {
        Ok(Some(ctrl)) => ctrl,
        Ok(None) => {
            log::warn!("micro: no root bridge decodes the controller");
            return None;
        },
        Err(e) => {
            log::warn!("micro: locating the controller failed: {}", Chain(&e));
            return None;
        },
    };
    let mut rb = match RootBridge::open(ctrl.bridge) {
        Ok(rb) => rb,
        Err(e) => {
            log::warn!("micro: opening the root bridge failed: {e}");
            return None;
        },
    };
    let dcctrl = dbc.dcctrl_addr();
    let mut acc = 0;
    let t0 = clock.now();
    for _ in 0..MICRO_READS {
        acc ^= rb.read32(dcctrl);
    }
    let read_total = clock.now() - t0;
    let direct_total = direct_reads(dcctrl, clock, &mut acc);
    black_box(acc);
    let t0 = clock.now();
    for _ in 0..MICRO_READS {
        dbc.poll(|_| {});
    }
    let poll_total = clock.now() - t0;

    logger::with(|log| {
        log.flush();
        let t0 = clock.now();
        log.line("bench micro: one log line");
        let t1 = clock.now();
        let flushed = log.flush();
        let t2 = clock.now();
        MicroResult { read_total, direct_total, poll_total, line: t1 - t0, flush: t2 - t1, flushed }
    })
}

/// The same DCCTRL reads as plain loads, to price the firmware root bridge call against the bus itself.
#[expect(
    clippy::disallowed_methods,
    reason = "measures direct MMIO against the root bridge path; never used for control"
)]
fn direct_reads(dcctrl: ferrite_hal::PhysAddr, clock: Clock, acc: &mut u32) -> Duration {
    let reg = dcctrl.get() as usize as *const u32;
    let t0 = clock.now();
    for _ in 0..MICRO_READS {
        // SAFETY: UEFI identity-maps MMIO, and DCCTRL is a 4-byte aligned register the driver itself reads.
        *acc ^= unsafe { core::ptr::read_volatile(reg) };
    }
    clock.now() - t0
}

fn log_results(r: &Results) {
    match &r.source {
        Some(s) => log::info!(
            "bench source: bytes={} frames={} elapsed={} queued_in={} rate={} refused_frames={} polls={} in_poll={}{}",
            s.bytes,
            s.frames,
            Ms(s.elapsed),
            Ms(s.queued_in),
            Rate { bytes: s.bytes, over: s.elapsed },
            s.refused_frames,
            s.polled.polls,
            Ms(s.polled.inside),
            incomplete(s.complete),
        ),
        None => log::warn!("bench source: not run"),
    }
    match &r.sink {
        Some(s) => log::info!(
            "bench sink: bytes={} window={} rate={} frames_ok={} crc_failures={} seq_gaps={} seq_missing={} resync_bytes={} polls={} in_poll={}{}",
            s.bytes,
            Ms(s.window),
            Rate { bytes: s.bytes, over: s.window },
            s.stats.frames_ok,
            s.stats.crc_failures,
            s.stats.gaps,
            s.stats.missing,
            s.stats.resync_bytes,
            s.polled.polls,
            Ms(s.polled.inside),
            incomplete(s.complete),
        ),
        None => log::warn!("bench sink: not run"),
    }
    match &r.echo {
        Some(e) => {
            let at = |pct| bench::percentile(&e.turnaround, pct).unwrap_or_default().as_nanos();
            log::info!(
                "bench echo: chunks={} bytes={} refused_bytes={} turnaround_ns p50={} p99={} max={} unsampled={} polls={} in_poll={}{}",
                e.chunks,
                e.bytes,
                e.refused_bytes,
                at(50),
                at(99),
                at(100),
                e.unsampled,
                e.polled.polls,
                Ms(e.polled.inside),
                incomplete(e.complete),
            );
        },
        None => log::warn!("bench echo: not run"),
    }
    match &r.micro {
        Some(m) => log::info!(
            "bench micro: dcctrl_reads={MICRO_READS} total={} per_read_ns={} direct_read_ns={} idle_poll_ns={} log_line={} log_flush={}{}",
            Ms(m.read_total),
            m.read_total.as_nanos() / u128::from(MICRO_READS),
            m.direct_total.as_nanos() / u128::from(MICRO_READS),
            m.poll_total.as_nanos() / u128::from(MICRO_READS),
            Ms(m.line),
            Ms(m.flush),
            if m.flushed { "" } else { " (flush failed)" },
        ),
        None => log::warn!("bench micro: not run"),
    }
}

const fn incomplete(complete: bool) -> &'static str {
    if complete { "" } else { " INCOMPLETE" }
}
