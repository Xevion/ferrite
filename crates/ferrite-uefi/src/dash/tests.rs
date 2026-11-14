use std::collections::VecDeque;
use std::vec;
use std::vec::Vec;

use assert2::{assert, check};
use ferrite_mem::{CpuAddr, Fault};
use smoltcp::wire::{Ipv4Address, Ipv4Cidr};

use super::*;
use crate::report::{CpuInfo, History, PassRecord, TestRecord};
use crate::sweep::Coverage;
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
        coverage: Coverage::NONE,
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
    // One percent more moves the bar's end and the figure, and the map's active cell.
    let moved = SweepStatus { test_percent: 38, ..status() };
    let rects = render(&mut dash, moved, &history, &faults, 500);
    check!(inside(&rects, &[l.test, l.map.body]));
    check!(area(&rects) * 5 < l.test.area());
    // A second later the run clock, the pass's time and the log line tick; nothing else does.
    let rects = render(&mut dash, moved, &history, &faults, 501);
    check!(inside(&rects, &[l.clock, l.pass, l.log]));
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
fn the_first_fault_shows_in_the_errors_the_tab_and_the_clock() {
    let mut dash = Dashboard::new(1280, 800).expect("dashboard");
    let (history, mut faults) = (history(), VecDeque::new());
    render(&mut dash, status(), &history, &faults, 500);
    let mut status = status();
    faulty(&mut status, &mut faults);
    let rects = render(&mut dash, status, &history, &faults, 500);
    let l = dash.layout;
    let places = [l.errors.body, l.tabs, l.clock, l.log];
    check!(!rects.is_empty() && rects.iter().all(|r| places.iter().any(|w| w.intersect(*r) == *r)), "{rects:?}");
    // The count is drawn in the error colour: some pixel of the errors section is exactly it.
    let wanted = [theme::ERR.b, theme::ERR.g, theme::ERR.r, 255];
    let width = l.errors.body.w as usize;
    let seen = (l.errors.body.y..l.errors.body.bottom()).any(|y| {
        let row = (y as usize * 1280 + l.errors.body.x as usize) * 4;
        dash.canvas.bgra()[row..row + width * 4].as_chunks::<4>().0.contains(&wanted)
    });
    assert!(seen, "no error coloured pixel in the errors section");
}

#[test]
fn too_small_a_screen_is_refused() {
    assert!(let Err(DashError::TooSmall { .. }) = Dashboard::new(800, 400));
}

#[test]
fn every_size_renders() {
    for (w, h) in [(768, 576), (1024, 768), (1280, 800), (1920, 1080)] {
        for faulted in [false, true] {
            let mut dash = Dashboard::new(w, h).expect("dashboard");
            let (history, mut faults) = (history(), VecDeque::new());
            let mut status = status();
            if faulted {
                faulty(&mut status, &mut faults);
            }
            check!(render(&mut dash, status, &history, &faults, 523) == [dash.canvas.bounds()]);
        }
    }
}

/// The frames as PNG files in the directory named by `FERRITE_DASH_OUT`, for looking at.
/// Run with `cargo nextest run -p ferrite-uefi --lib --run-ignored only dash::tests::png_frames`.
#[test]
#[ignore = "writes files; run on demand for a look at the screen"]
fn png_frames() {
    let dir =
        std::path::PathBuf::from(std::env::var_os("FERRITE_DASH_OUT").expect("FERRITE_DASH_OUT names a directory"));
    std::fs::create_dir_all(&dir).expect("create the output directory");
    let no_network = Some(NoNetwork { wired: "no wired NIC driven by the firmware", wifi: "scanning" });
    for (w, h) in [(1920, 1080), (1024, 768)] {
        for scene in ["running", "fault", "no-network"] {
            let mut dash = Dashboard::new(w, h).expect("dashboard");
            let (history, mut faults) = (history(), VecDeque::new());
            let mut status = status();
            if scene == "fault" {
                faulty(&mut status, &mut faults);
            }
            let snapshot = Snapshot { status, history: &history, faults: &faults };
            let (net, net_err) = if scene == "no-network" { (None, no_network) } else { (Some(net()), None) };
            let at = START + Duration::from_secs(523);
            dash.render(&Frame { snapshot: &snapshot, net, net_err, now: at });
            let bytes = png(w, h, dash.canvas.bgra());
            std::fs::write(dir.join(std::format!("{scene}-{w}x{h}.png")), bytes).expect("write frame");
        }
    }
}

/// An uncompressed RGB PNG of BGRA `pixels`.
fn png(w: u32, h: u32, pixels: &[u8]) -> Vec<u8> {
    fn chunk(out: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
        out.extend((data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend(data);
        out.extend(&body);
        let mut crc = !0u32;
        for byte in body {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 { 0xedb8_8320 ^ (crc >> 1) } else { crc >> 1 };
            }
        }
        out.extend((!crc).to_be_bytes());
    }
    let mut raw = Vec::with_capacity((w as usize * 3 + 1) * h as usize);
    for row in pixels.chunks_exact(w as usize * 4) {
        raw.push(0);
        raw.extend(row.as_chunks::<4>().0.iter().flat_map(|p| [p[2], p[1], p[0]]));
    }
    let mut zlib = vec![0x78, 0x01];
    let blocks = raw.chunks(65_535).len();
    for (i, block) in raw.chunks(65_535).enumerate() {
        zlib.push(u8::from(i + 1 == blocks));
        zlib.extend((block.len() as u16).to_le_bytes());
        zlib.extend((!(block.len() as u16)).to_le_bytes());
        zlib.extend(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in &raw {
        a = (a + u32::from(byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    zlib.extend(((b << 16) | a).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::new();
    header.extend(w.to_be_bytes());
    header.extend(h.to_be_bytes());
    header.extend([8, 2, 0, 0, 0]);
    chunk(&mut out, *b"IHDR", &header);
    chunk(&mut out, *b"IDAT", &zlib);
    chunk(&mut out, *b"IEND", &[]);
    out
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
    check!(!rects.is_empty() && rects.iter().all(|r| l.net.body.intersect(*r) == *r), "{rects:?}");

    dash.render(&frame(None, Some(NoNetwork { wired: "no wired NIC driven by the firmware", wifi: "scanning" })));
    let rects = dash.output().1.to_vec();
    check!(!rects.is_empty() && rects.iter().all(|r| l.net.body.intersect(*r) == *r), "{rects:?}");
}
