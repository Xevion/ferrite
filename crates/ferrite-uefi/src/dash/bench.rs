//! Host profile of the dashboard over a realistic frame sequence: time per widget and per stage,
//! and heap allocations per frame. Run with
//! `cargo nextest run -p ferrite-uefi --lib --run-ignored only dash::bench --no-capture`; the
//! dev profile is what the apps are built with. Environment:
//!
//! - `FERRITE_DASH_FRAMES`: frames to render (2600, a little over two minutes at 20 Hz).
//! - `FERRITE_DASH_NOISE`: threads streaming over a large buffer, standing in for the sweep's
//!   workers; the target's frames run against that memory traffic.
//! - `FERRITE_DASH_LEAN`: only time `render`, with no per-stage profile, hashes or frame dumps.
//! - `FERRITE_DASH_OUT`: a directory for a hash per frame and PPM frames, to compare two builds.

#![expect(
    clippy::many_single_char_names,
    clippy::too_many_lines,
    clippy::cast_precision_loss,
    reason = "a profile script: short names, one long report, counts as floats"
)]

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::string::String;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration as Elapsed, Instant as Clock};
use std::vec::Vec;

use assert2::check;
use ferrite_mem::{CpuAddr, Fault};
use smoltcp::wire::{Ipv4Address, Ipv4Cidr};

use super::*;
use crate::gfx::rect::{Plane, changed_bands};
use crate::pace::Outlook;
use crate::pace::TestStats;
use crate::report::{CpuInfo, History, PassRecord, TestRecord};
use crate::sweep::Coverage;
use ferrite_net::{Link, MacAddr, Medium, PortStats, PortStatus, ServiceCounters};

pub(super) use crate::testing::allocations;

const START: Instant = Instant::from_epoch(Duration::from_secs(5));
const FRAME: Duration = Duration::from_millis(50);
/// Frames one test runs for.
const TEST_FRAMES: u32 = 160;
/// The first fault is found at this frame, and another every `FAULT_EVERY`.
const FAULTS_FROM: u32 = 1000;
const FAULT_EVERY: u32 = 200;

/// A sweep advancing at 20 frames a second: the test percentage climbs, the clock ticks, tests
/// finish and then a pass, and faults turn up.
struct Scenario {
    status: SweepStatus,
    history: History,
    faults: VecDeque<FaultRecord>,
    frame: u32,
    kinds: Vec<TestKind>,
}

impl Scenario {
    fn new() -> Self {
        let kinds: Vec<TestKind> = TestKind::sequence(Preset::Standard).collect();
        let mut history = History::default();
        history.run_started = Some(START);
        history.pass_started = Some(START);
        history.cpus = (0..8)
            .map(|n| CpuInfo { number: n, package: 0, core: (n / 2) as u32, thread: (n % 2) as u32, bsp: n == 0 })
            .collect();
        let status = SweepStatus {
            bytes: 1 << 30,
            test: kinds[0].name(),
            position: (1, kinds.len()),
            test_percent: 0,
            percent: 0,
            passes: 0,
            tier: "avx2",
            mode: Mode::Split,
            cpus: 8,
            faults: 0,
            first_fault: None,
            analysis: Classification::Clean,
            preset: Preset::Standard,
            paused: false,
            coverage: Coverage::NONE,
            outlook: Outlook::NONE,
        };
        Self { status, history, faults: VecDeque::new(), frame: 0, kinds }
    }

    fn now(&self) -> Instant {
        START + FRAME * self.frame
    }

    /// The network, down for a stretch.
    fn net(&self) -> (Option<NetStatus>, Option<NoNetwork>) {
        if (2000..2060).contains(&self.frame) {
            (None, Some(NoNetwork { wired: "link is down", wifi: "connecting" }))
        } else {
            (Some(net()), None)
        }
    }

    fn advance(&mut self) {
        self.frame += 1;
        let n = self.kinds.len() as u32;
        let in_test = self.frame % TEST_FRAMES;
        let done_tests = self.frame / TEST_FRAMES;
        let (pass, index) = (u64::from(done_tests / n), done_tests % n);
        if in_test == 0 {
            let kind = self.kinds[((done_tests + n - 1) % n) as usize];
            self.history.tests.push_back(TestRecord {
                pass: pass.saturating_sub(u64::from(index == 0)),
                test: kind.name(),
                cpus: 8,
                took: FRAME * TEST_FRAMES,
                work_bytes: 6 << 30,
                faults: 0,
            });
            self.history.revision += 1;
            if index == 0 {
                self.history.passes.push_back(PassRecord {
                    pass: pass - 1,
                    took: FRAME * (TEST_FRAMES * n),
                    faults: 0,
                });
                self.history.pass_started = Some(self.now());
            }
        }
        if self.frame >= FAULTS_FROM && (self.frame - FAULTS_FROM).is_multiple_of(FAULT_EVERY) {
            let at = self.frame as usize * 64;
            let expected = 0xaaaa_aaaa_5555_5555u64;
            let fault =
                Fault { addr: CpuAddr::of(at as *const u64), expected, actual: expected ^ (1 << (self.frame % 60)) };
            let record = FaultRecord { fault, test: self.kinds[index as usize].name(), cpu: 2, pass };
            self.faults.push_back(record);
            self.status.faults += 1;
            self.status.first_fault.get_or_insert(record);
            self.status.analysis = Classification::Mixed { stuck: 1 << 3 };
        }
        let pass_frames = TEST_FRAMES * n;
        self.status.paused = (1700..1760).contains(&self.frame);
        self.status.passes = pass;
        self.status.test = self.kinds[index as usize].name();
        self.status.position = (index as usize + 1, self.kinds.len());
        self.status.test_percent = (in_test * 100 / TEST_FRAMES) as u16;
        self.status.percent = ((self.frame % pass_frames) * 100 / pass_frames) as u16;
    }
}

fn net() -> NetStatus {
    let mut net = NetStatus::default();
    net.push(PortStatus {
        link: Link {
            medium: Medium::Ethernet,
            up: true,
            mac: MacAddr::new([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]),
            address: Some(Ipv4Cidr::new(Ipv4Address::new(10, 0, 2, 15), 24)),
            gateway: Some(Ipv4Address::new(10, 0, 2, 2)),
            dns: Some(Ipv4Address::new(10, 0, 2, 3)),
            ..Link::default()
        },
        frames: PortStats::default(),
        services: ServiceCounters::default(),
    });
    net
}

fn hash(pixels: &[u8]) -> u64 {
    pixels.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

fn ms(d: Elapsed) -> f64 {
    d.as_secs_f64() * 1e3
}

/// A thread streaming over a buffer far bigger than the caches, as the sweep's workers do, to make
/// the dashboard's memory traffic compete for the memory controller.
fn hammer(stop: Arc<AtomicBool>) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut buf = std::vec![0u64; 1 << 25];
        let mut seed = 1u64;
        while !stop.load(Ordering::Relaxed) {
            for word in &mut buf {
                seed = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
                *word ^= seed;
            }
            std::hint::black_box(&buf);
        }
    })
}

fn env<T: std::str::FromStr>(name: &str) -> Option<T> {
    std::env::var(name).ok().and_then(|v| v.parse().ok())
}

/// Time a widget spent in the two halves of a redraw: running its drawing code, which records
/// steps, and `Canvas::finish`, which finds what differs and draws and copies it.
#[derive(Default, Clone, Copy)]
struct Spent {
    draws: u32,
    record: Elapsed,
    finish: Elapsed,
}

#[derive(Default)]
struct Profile {
    widgets: [Spent; Widget::ALL.len()],
    backdrop: (u32, Elapsed),
    keys: Elapsed,
}

/// [`Dashboard::render`] with a clock around each part, on a second dashboard. The caller checks
/// the two draw the same.
fn timed_render(dash: &mut Dashboard, f: &Frame<'_>, prof: &mut Profile) {
    dash.dirty.clear();
    let t = Clock::now();
    let bounds = dash.canvas.bounds();
    let keys = Keys::of(f, (bounds.w, bounds.h));
    prof.keys += t.elapsed();
    let last = dash.last.filter(|last| last.setup == keys.setup);
    let full = last.is_none();
    if full {
        let t = Clock::now();
        dash.layout = Layout::new(bounds.w, bounds.h);
        dash.pen().backdrop();
        dash.dirty.add_all();
        prof.backdrop.0 += 1;
        prof.backdrop.1 += t.elapsed();
    }
    let l = dash.layout;
    let band = (bounds.w / 24).max(64);
    for widget in Widget::ALL {
        if !keys.changed(last.as_ref(), widget) {
            continue;
        }
        let rect = widget.rect(&l);
        let mut pen = Pen { canvas: &mut dash.canvas, fonts: &mut dash.fonts, l: &l };
        let t = Clock::now();
        pen.begin(rect);
        widget.draw()(&mut pen, rect, f);
        let record = t.elapsed();
        let t = Clock::now();
        dash.canvas.finish(&mut dash.fonts, &mut dash.drawn[widget as usize], full, &mut dash.dirty, band);
        let spent = &mut prof.widgets[widget as usize];
        spent.draws += 1;
        spent.record += record;
        spent.finish += t.elapsed();
    }
    dash.last = Some(keys);
}

/// The cost of a full redraw, which the first frame, the first fault and a failed push each cause.
#[test]
#[ignore = "profile, not a check: run by name"]
fn profile_full_redraws() {
    let rounds: u32 = env("FERRITE_DASH_FRAMES").unwrap_or(200);
    let mut scenario = Scenario::new();
    for _ in 0..300 {
        scenario.advance();
    }
    let mut dash = Dashboard::new(1920, 1080).expect("dashboard");
    let snapshot = Snapshot {
        status: scenario.status,
        history: &scenario.history,
        faults: &scenario.faults,
        stats: &TestStats::default(),
    };
    let (net, net_err) = scenario.net();
    let frame = Frame { snapshot: &snapshot, net, net_err, now: scenario.now(), time: None };
    dash.render(&frame);
    let mut costs = Vec::new();
    for _ in 0..rounds {
        dash.invalidate();
        let t = Clock::now();
        dash.render(&frame);
        costs.push(t.elapsed());
    }
    costs.sort();
    std::println!(
        "full redraw: mean {:.3} ms, p50 {:.3}, max {:.3}",
        ms(costs.iter().sum::<Elapsed>() / rounds),
        ms(costs[costs.len() / 2]),
        ms(costs[costs.len() - 1])
    );
}

#[test]
#[ignore = "profile, not a check: run by name"]
fn profile_a_sweep() {
    let out = std::env::var_os("FERRITE_DASH_OUT").map(std::path::PathBuf::from);
    let frames: u32 = env("FERRITE_DASH_FRAMES").unwrap_or(2600);
    let lean = std::env::var_os("FERRITE_DASH_LEAN").is_some();
    let (w, h) = (1920, 1080);
    let stop = Arc::new(AtomicBool::new(false));
    let hammers: Vec<_> = (0..env("FERRITE_DASH_NOISE").unwrap_or(0)).map(|_| hammer(Arc::clone(&stop))).collect();

    let mut scenario = Scenario::new();
    let new = Clock::now();
    let mut dash = Dashboard::new(w, h).expect("dashboard");
    let new = new.elapsed();
    let mut twin = Dashboard::new(w, h).expect("dashboard");
    let mut prof = Profile::default();

    let mut hashes = String::new();
    let mut costs: Vec<Elapsed> = Vec::new();
    let (mut first, mut full_frames) = (Elapsed::ZERO, Vec::<Elapsed>::new());
    let (mut allocs, mut bytes, mut worst_allocs) = (0u64, 0u64, 0u64);
    let (mut pushed, mut whole_widgets, mut prev) = (0u64, 0u64, Vec::<u8>::new());
    for i in 0..frames {
        if i > 0 {
            scenario.advance();
        }
        let snapshot = Snapshot {
            status: scenario.status,
            history: &scenario.history,
            faults: &scenario.faults,
            stats: &TestStats::default(),
        };
        let (net, net_err) = scenario.net();
        let frame = Frame { snapshot: &snapshot, net, net_err, now: scenario.now(), time: None };

        let before = allocations();
        let t = Clock::now();
        dash.render(&frame);
        let took = t.elapsed();
        let after = allocations();

        let rects = dash.output().1;
        let full = rects == [dash.canvas.bounds()];
        let area: u64 = rects.iter().map(|r| r.area()).sum();
        if i == 0 {
            first = took;
        } else if full {
            full_frames.push(took);
        } else if !rects.is_empty() {
            costs.push(took);
            pushed += area;
            allocs += after.0 - before.0;
            bytes += after.1 - before.1;
            worst_allocs = worst_allocs.max(after.0 - before.0);
        }
        if lean {
            continue;
        }

        // What diffing each whole widget against the previous frame would push.
        let l = dash.layout;
        let mut whole = DirtyRects::new(dash.canvas.bounds());
        if prev.len() == dash.canvas.bgra().len() {
            let stride = w as usize * 4;
            for r in Widget::ALL.map(|widget| widget.rect(&l)) {
                let at = (r.y as usize * w as usize + r.x as usize) * 4;
                let old = Plane { data: &prev[at..], stride };
                let new = Plane { data: &dash.canvas.bgra()[at..], stride };
                for found in changed_bands(old, new, r, (w / 24).max(64)) {
                    whole.add(found);
                }
            }
        }
        if !full && i > 0 {
            whole_widgets += whole.as_slice().iter().map(|r| r.area()).sum::<u64>();
        }
        prev.clear();
        prev.extend_from_slice(dash.canvas.bgra());

        timed_render(&mut twin, &frame, &mut prof);
        let (a, b) = (hash(dash.canvas.bgra()), hash(twin.canvas.bgra()));
        check!(a == b, "frame {i}: the profiled render drew something else");
        check!(dash.output().1 == twin.output().1, "frame {i}: the profiled render pushes other rects");
        writeln!(hashes, "{i} {a:016x}").ok();
        if let Some(dir) = &out
            && [0, 500, 1100, 1500, 1730, 2030, frames - 1].contains(&i)
        {
            let mut ppm = std::format!("P6 {w} {h} 255\n").into_bytes();
            ppm.extend(dash.canvas.bgra().as_chunks::<4>().0.iter().flat_map(|p| [p[2], p[1], p[0]]));
            std::fs::write(dir.join(std::format!("sweep-{i:04}.ppm")), ppm).expect("write frame");
        }
    }
    if let Some(dir) = &out {
        std::fs::write(dir.join("hashes.txt"), hashes).expect("write hashes");
    }
    stop.store(true, Ordering::Relaxed);
    for thread in hammers {
        thread.join().expect("hammer thread");
    }

    costs.sort();
    let n = costs.len().max(1);
    let mean = costs.iter().sum::<Elapsed>() / n as u32;
    let pct = |p: usize| costs.get(n * p / 100).copied().unwrap_or_default();
    if lean {
        std::println!(
            "LEAN partial frames {} mean {:.4} ms p50 {:.4} p90 {:.4} p99 {:.4} allocs/frame {:.2} KiB/frame {:.2}",
            costs.len(),
            ms(mean),
            ms(pct(50)),
            ms(pct(90)),
            ms(pct(99)),
            allocs as f64 / n as f64,
            bytes as f64 / n as f64 / 1024.0
        );
        return;
    }
    std::println!("\nscenario: {frames} frames at 20 Hz, {w}x{h}");
    std::println!("Dashboard::new {:.2} ms; first frame {:.2} ms", ms(new), ms(first));
    std::println!(
        "full redraws after the first: {} (mean {:.2} ms)",
        full_frames.len(),
        ms(full_frames.iter().sum::<Elapsed>() / full_frames.len().max(1) as u32)
    );
    std::println!(
        "partial frames: {}; mean {:.3} ms, p50 {:.3}, p90 {:.3}, p99 {:.3}, max {:.3} ms",
        costs.len(),
        ms(mean),
        ms(pct(50)),
        ms(pct(90)),
        ms(pct(99)),
        ms(costs.last().copied().unwrap_or_default()),
    );
    std::println!(
        "pixels pushed per partial frame: {:.0} (diffing whole widgets: {:.0})",
        pushed as f64 / n as f64,
        whole_widgets as f64 / n as f64
    );
    std::println!(
        "heap, partial frames: {:.1} allocations ({:.1} KiB) per frame, worst {worst_allocs}",
        allocs as f64 / n as f64,
        bytes as f64 / n as f64 / 1024.0
    );
    std::println!(
        "\n{:8} {:>6} {:>10} {:>10} {:>10} {:>10}",
        "widget",
        "draws",
        "record",
        "finish",
        "total ms",
        "mean ms"
    );
    let mut widgets: Vec<_> = Widget::ALL.into_iter().zip(prof.widgets).collect();
    widgets.sort_by_key(|(_, spent)| std::cmp::Reverse(spent.record + spent.finish));
    let (mut record, mut finish) = (Elapsed::ZERO, Elapsed::ZERO);
    for (widget, spent) in widgets {
        let total = spent.record + spent.finish;
        (record, finish) = (record + spent.record, finish + spent.finish);
        std::println!(
            "{:8} {:>6} {:>10.2} {:>10.2} {:>10.2} {:>10.3}",
            std::format!("{widget:?}"),
            spent.draws,
            ms(spent.record),
            ms(spent.finish),
            ms(total),
            ms(total) / f64::from(spent.draws.max(1)),
        );
    }
    let grand = ms(record + finish);
    std::println!(
        "record {:.2} ms ({:.1}%), finish {:.2} ms ({:.1}%)",
        ms(record),
        100.0 * ms(record) / grand,
        ms(finish),
        100.0 * ms(finish) / grand
    );
    std::println!(
        "backdrop {:.2} ms over {} layouts; keys {:.2} ms",
        ms(prof.backdrop.1),
        prof.backdrop.0,
        ms(prof.keys)
    );
}
