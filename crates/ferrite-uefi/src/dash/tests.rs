use std::collections::VecDeque;
use std::vec::Vec;

use assert2::{assert, check};
use ferrite_mem::{CpuAddr, Fault};
use smoltcp::wire::{Ipv4Address, Ipv4Cidr};

use super::*;
use crate::report::{CpuInfo, History, PassRecord, TestRecord};
use ferrite_net::{Link, MacAddr, Medium, PortStats, PortStatus, ServiceCounters};

const START: Instant = Instant::from_epoch(Duration::from_secs(5));

fn status() -> SweepStatus {
    SweepStatus {
        bytes: 1 << 30,
        test: TestKind::MarchCMinus.name(),
        position: (5, 14),
        test_percent: 37,
        percent: 41,
        passes: 3,
        tier: "avx2",
        mode: Mode::Split,
        cpus: 8,
        faults: 0,
        first_fault: None,
        analysis: Classification::Clean,
        preset: Preset::Standard,
        paused: false,
    }
}

fn history() -> History {
    let mut history = History::default();
    history.revision = 40;
    history.run_started = Some(START);
    history.pass_started = Some(START + Duration::from_secs(400));
    for pass in 0..4u64 {
        for (i, kind) in TestKind::sequence(Preset::Standard).enumerate() {
            if pass == 3 && i >= 4 {
                break;
            }
            let i = i as u64;
            history.tests.push_back(TestRecord {
                pass,
                test: kind.name(),
                cpus: if i.is_multiple_of(3) { 1 } else { 8 },
                took: Duration::from_millis(900 + i * 731 + pass * 40),
                work_bytes: (3 + i) << 30,
                faults: 0,
            });
        }
        if pass < 3 {
            let took = Duration::from_secs(128 + pass * 3);
            history.passes.push_back(PassRecord { pass, took, faults: 0 });
        }
    }
    history.cpus = (0..8)
        .map(|n| CpuInfo { number: n, package: 0, core: (n / 2) as u32, thread: (n % 2) as u32, bsp: n == 0 })
        .collect();
    history
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
        },
        frames: PortStats::default(),
        services: ServiceCounters::default(),
    });
    net
}

fn faulty(status: &mut SweepStatus, faults: &mut VecDeque<FaultRecord>) {
    for (n, bit) in [(0x1_2340_0000usize, 3u32), (0x1_2340_0040, 3), (0x2_0000_1000, 41)] {
        let expected = 0xaaaa_aaaa_5555_5555u64;
        let fault = Fault { addr: CpuAddr::of(n as *const u64), expected, actual: expected ^ (1 << bit) };
        faults.push_back(FaultRecord { fault, test: TestKind::MovingInversions.name(), cpu: 2, pass: 2 });
    }
    status.faults = 3;
    status.first_fault = Some(faults[0]);
    status.analysis = Classification::Mixed { stuck: 1 << 3 };
}

fn render(
    dash: &mut Dashboard,
    status: SweepStatus,
    history: &History,
    faults: &VecDeque<FaultRecord>,
    at: u64,
) -> Vec<Rect> {
    let snapshot = Snapshot { status, history, faults };
    let frame = Frame { snapshot: &snapshot, net: Some(net()), net_err: None, now: START + Duration::from_secs(at) };
    dash.render(&frame);
    dash.output().1.to_vec()
}

#[test]
fn steady_frames_redraw_only_the_widgets_that_changed() {
    let mut dash = Dashboard::new(1920, 1080).expect("dashboard");
    let (history, faults) = (history(), VecDeque::new());
    let full = render(&mut dash, status(), &history, &faults, 500);
    check!(full == [dash.canvas.bounds()]);
    check!(render(&mut dash, status(), &history, &faults, 500).is_empty());

    let l = dash.layout;
    let inside = |rects: &[Rect], widgets: &[Rect]| {
        !rects.is_empty() && rects.iter().all(|r| widgets.iter().any(|w| w.intersect(*r) == *r))
    };
    let area = |rects: &[Rect]| rects.iter().map(|r| r.area()).sum::<u64>();
    // One percent more moves the bar's end and the figure: a sliver of the widget.
    let moved = SweepStatus { test_percent: 38, ..status() };
    let rects = render(&mut dash, moved, &history, &faults, 500);
    check!(inside(&rects, &[l.test]));
    check!(area(&rects) * 5 < l.test.area());
    // A second later the run clock and the pass's times tick; nothing else does.
    let rects = render(&mut dash, moved, &history, &faults, 501);
    check!(inside(&rects, &[l.clock, l.pass]));
    check!(rects.iter().any(|r| l.clock.intersect(*r) == *r));
}

#[test]
fn a_clock_tick_allocates_nothing() {
    let mut dash = Dashboard::new(1920, 1080).expect("dashboard");
    let (history, faults) = (history(), VecDeque::new());
    // The first frames size the scratch buffer and rasterize each digit the clocks show.
    for at in 500..560 {
        render(&mut dash, status(), &history, &faults, at);
    }
    let snapshot = Snapshot { status: status(), history: &history, faults: &faults };
    let frame = Frame { snapshot: &snapshot, net: Some(net()), net_err: None, now: START + Duration::from_secs(560) };
    let before = bench::allocations().0;
    dash.render(&frame);
    let allocated = bench::allocations().0 - before;
    check!(!dash.output().1.is_empty());
    check!(allocated == 0);
}

#[test]
fn the_first_fault_redraws_everything() {
    let mut dash = Dashboard::new(1280, 800).expect("dashboard");
    let (history, mut faults) = (history(), VecDeque::new());
    render(&mut dash, status(), &history, &faults, 500);
    let mut status = status();
    faulty(&mut status, &mut faults);
    check!(render(&mut dash, status, &history, &faults, 500) == [dash.canvas.bounds()]);
    // The header's ground has turned red.
    let l = dash.layout;
    let at = ((l.header.y + 3) as usize * 1280 + (l.header.x + l.header.w / 2) as usize) * 4;
    let [b, g, r, _] = dash.canvas.bgra()[at..at + 4] else { panic!("pixel out of range") };
    assert!(r > g && r > b, "header pixel {r},{g},{b} is not red");
}

#[test]
fn too_small_a_screen_is_refused() {
    assert!(let Err(DashError::TooSmall { .. }) = Dashboard::new(800, 400));
}

/// Writes PPM frames under `FERRITE_DASH_OUT` for looking at; without it only checks they render.
#[test]
fn eyeball_frames() {
    let out = std::env::var_os("FERRITE_DASH_OUT").map(std::path::PathBuf::from);
    for (w, h) in [(1280, 800), (1920, 1080), (1024, 768)] {
        for faulted in [false, true] {
            let mut dash = Dashboard::new(w, h).expect("dashboard");
            let (history, mut faults) = (history(), VecDeque::new());
            let mut status = status();
            if faulted {
                faulty(&mut status, &mut faults);
            }
            render(&mut dash, status, &history, &faults, 523);
            let Some(dir) = &out else { continue };
            let mut ppm = std::format!("P6 {w} {h} 255\n").into_bytes();
            ppm.extend(dash.canvas.bgra().as_chunks::<4>().0.iter().flat_map(|p| [p[2], p[1], p[0]]));
            let name = std::format!("dash-{w}x{h}{}.ppm", if faulted { "-faulted" } else { "" });
            std::fs::write(dir.join(name), ppm).expect("write frame");
        }
    }
}

#[test]
fn a_port_coming_and_going_redraws_the_network_widget_alone() {
    let mut dash = Dashboard::new(1920, 1080).expect("dashboard");
    let (history, faults) = (history(), VecDeque::new());
    let snapshot = Snapshot { status: status(), history: &history, faults: &faults };
    let frame = |net: Option<NetStatus>, net_err| Frame { snapshot: &snapshot, net, net_err, now: START };

    dash.render(&frame(Some(net()), None));
    let mut two = net();
    two.push(PortStatus {
        link: Link {
            medium: Medium::WiFi,
            up: true,
            mac: MacAddr::new([0x02, 0, 0, 0, 0, 9]),
            address: None,
            ..Link::default()
        },
        ..PortStatus::default()
    });
    dash.render(&frame(Some(two), None));
    let l = dash.layout;
    let rects = dash.output().1.to_vec();
    check!(!rects.is_empty() && rects.iter().all(|r| l.net.intersect(*r) == *r), "{rects:?}");

    dash.render(&frame(None, Some(NoNetwork { wired: "no wired NIC driven by the firmware", wifi: "scanning" })));
    let rects = dash.output().1.to_vec();
    check!(!rects.is_empty() && rects.iter().all(|r| l.net.intersect(*r) == *r), "{rects:?}");
}
