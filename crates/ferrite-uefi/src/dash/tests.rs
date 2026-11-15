use std::collections::VecDeque;
use std::vec;
use std::vec::Vec;

use assert2::{assert, check};
use ferrite_mem::{CpuAddr, Fault};
use ferrite_power::units::Mhz;
use rstest::rstest;
use smoltcp::wire::{Ipv4Address, Ipv4Cidr};

use super::*;
use crate::report::{CpuInfo, History, PassRecord, TestRecord};
use core::num::NonZeroU64;

use ferrite_mem::eta::{BasisPoints, Confidence, Eta, Position};
use ferrite_mem::histogram::Histogram;

use ferrite_config::ClockFormat;

use crate::goal::Goal;
use crate::hardware::{Facts, Hardware, Vitals};
use crate::memmap::CellState;
use crate::pace::{Delta, Outlook, Steadiness, TestStat, TestStats};
use crate::sweep::Coverage;
use crate::wall::{Day, Ends, TimeLine, TimeOfDay};
use ferrite_net::{Link, MacAddr, Medium, PortStats, PortStatus, ServiceCounters};

const START: Instant = Instant::from_epoch(Duration::from_secs(5));

fn bp(raw: u16) -> BasisPoints {
    BasisPoints::new(raw).expect("a fraction")
}

/// A test running at 8 GB/s, a little slower than it was, in a pass with a guessed time left.
fn running() -> Outlook {
    Outlook {
        rate: Some(7_900_000_000),
        test_avg: Some(7_950_000_000),
        test_delta: Delta::between(7_950_000_000, 8_400_000_000),
        test_left: Some(Duration::from_secs(38)),
        test_progress: bp(3712),
        pass: Some(Eta { remaining: Duration::from_mins(34), confidence: Confidence::Estimated }),
        pass_progress: bp(4137),
        known: (14, 14),
        pace_percent: Some(98),
        bandwidth: Some(18_400_000_000),
        ..Outlook::NONE
    }
}

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
        outlook: running(),
        map: crate::testing::sample_map(40),
    }
}

/// What the four tests that finished in pass 3 measured: one with a rate that changed, one that
/// swings between two speeds, and a long name's worth of rates in GB/s and in MB/s.
fn finished_tests() -> TestStats {
    let rates = |samples: &[u64]| {
        let mut hist = Histogram::new();
        for &s in samples {
            hist.record(s);
        }
        Steadiness::of(&hist)
    };
    let steady =
        rates(&[12_000_000_000, 12_100_000_000, 11_900_000_000, 12_050_000_000, 11_950_000_000, 12_000_000_000]);
    let swinging = rates(&[6_000_000_000, 9_000_000_000, 6_100_000_000, 9_100_000_000, 6_000_000_000, 9_000_000_000]);
    let slow = rates(&[640_000_000, 650_000_000, 645_000_000, 655_000_000, 635_000_000]);
    let mut finished = TestStats::default();
    finished.reset(14);
    let rows = [
        (11_800_000_000, 12_300_000_000, steady),
        (8_000_000_000, 8_050_000_000, swinging),
        (645_000_000, 600_000_000, slow),
        (2_500_000_000, 2_500_000_000, None),
    ];
    for (i, (kind, (rate, saved, steadiness))) in TestKind::sequence(Preset::Standard).zip(rows).enumerate() {
        let delta = Delta::between(rate, saved);
        finished.set(i, TestStat { test: kind.name(), pass: 3, rate: Some(rate), delta, steadiness });
    }
    finished
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
            ..Link::default()
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
    status.map.cells[50] = CellState::Fault;
    status.faults = 3;
    status.first_fault = Some(faults[0]);
    status.analysis = Classification::Mixed { stuck: 1 << 3 };
}

#[expect(clippy::large_types_passed_by_value, reason = "every caller builds a fresh status for the call")]
fn render(
    dash: &mut Dashboard,
    status: SweepStatus,
    history: &History,
    faults: &VecDeque<FaultRecord>,
    at: u64,
) -> Vec<Rect> {
    let finished = finished_tests();
    let snapshot = Snapshot { status, history, faults, stats: &finished };
    let hardware = Hardware::default();
    let frame = Frame {
        snapshot: &snapshot,
        net: Some(net()),
        net_err: None,
        now: START + Duration::from_secs(at),
        time: None,
        hardware: &hardware,
    };
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
    // One percent more moves the bar's end and the figure.
    let outlook = Outlook { test_progress: bp(3812), ..running() };
    let moved = SweepStatus { test_percent: 38, outlook, ..status() };
    let rects = render(&mut dash, moved, &history, &faults, 500);
    check!(inside(&rects, &[l.test]));
    check!(area(&rects) * 5 < l.test.area());
    // A second later the run clock, the pass's time and the log line tick; nothing else does.
    let rects = render(&mut dash, moved, &history, &faults, 501);
    check!(inside(&rects, &[l.clock, l.pass, l.log]));
    check!(rects.iter().any(|r| l.clock.intersect(*r) == *r));
}

#[test]
fn a_notice_takes_the_log_line_for_a_while_and_redraws_it_alone() {
    let mut dash = Dashboard::new(1920, 1080).expect("dashboard");
    let (history, faults) = (history(), VecDeque::new());
    render(&mut dash, status(), &history, &faults, 500);
    let notice =
        Notice { text: format::fixed(format_args!("report saved: \\EFI\\ferrite\\reports\\x.txt")), failed: false };
    dash.notify(notice, START + Duration::from_secs(500));
    let rects = render(&mut dash, status(), &history, &faults, 500);
    let l = dash.layout;
    check!(!rects.is_empty() && rects.iter().all(|r| l.log.intersect(*r) == *r), "{rects:?}");
    check!(render(&mut dash, status(), &history, &faults, 500).is_empty());

    render(&mut dash, status(), &history, &faults, 521);
    check!(dash.notice.is_none(), "it goes after twenty seconds");
}

#[test]
fn a_core_moving_on_to_the_next_cell_redraws_the_memory_map_alone() {
    let mut dash = Dashboard::new(1920, 1080).expect("dashboard");
    let (history, faults) = (history(), VecDeque::new());
    render(&mut dash, status(), &history, &faults, 500);
    let l = dash.layout;
    let moved = SweepStatus { map: crate::testing::sample_map(41), ..status() };
    let rects = render(&mut dash, moved, &history, &faults, 500);
    check!(!rects.is_empty() && rects.iter().all(|r| l.map.body.intersect(*r) == *r), "{rects:?}");
    check!(rects.iter().map(|r| r.area()).sum::<u64>() * 8 < l.map.body.area(), "only the two cells that changed");
}

#[test]
fn a_hundredth_of_a_percent_moves_the_pass_figure_and_bar_alone() {
    let mut dash = Dashboard::new(1920, 1080).expect("dashboard");
    let (history, faults) = (history(), VecDeque::new());
    render(&mut dash, status(), &history, &faults, 500);
    let mut moved = status();
    moved.outlook.pass_progress = bp(4138);
    let rects = render(&mut dash, moved, &history, &faults, 500);
    let l = dash.layout;
    check!(!rects.is_empty() && rects.iter().all(|r| l.pass.intersect(*r) == *r), "{rects:?}");
}

#[test]
fn the_bar_of_a_short_goal_moves_a_quarter_second_at_a_time_while_the_clocks_wait_for_the_second() {
    let mut dash = Dashboard::new(1920, 1080).expect("dashboard");
    let (history, faults, finished) = (history(), VecDeque::new(), finished_tests());
    let goal = Goal { passes: None, time: Some(Duration::from_mins(10)), stop_on_fault: false };
    let status = SweepStatus { outlook: Outlook { goal, ..running() }, ..status() };
    let snapshot = Snapshot { status, history: &history, faults: &faults, stats: &finished };
    let hardware = Hardware::default();
    let at = |millis| Frame {
        snapshot: &snapshot,
        net: Some(net()),
        net_err: None,
        now: START + Duration::from_millis(millis),
        time: None,
        hardware: &hardware,
    };
    dash.render(&at(500_000));
    dash.render(&at(500_100));
    check!(dash.output().1.is_empty(), "within the same quarter second nothing is drawn");
    dash.render(&at(500_300));
    let l = dash.layout;
    let rects = dash.output().1.to_vec();
    check!(!rects.is_empty() && rects.iter().all(|r| l.pass.intersect(*r) == *r), "{rects:?}");
}

#[test]
fn a_clock_tick_allocates_nothing() {
    let mut dash = Dashboard::new(1920, 1080).expect("dashboard");
    let (history, faults, finished) = (history(), VecDeque::new(), finished_tests());
    let snapshot = Snapshot { status: status(), history: &history, faults: &faults, stats: &finished };
    // The time of day ticks with the run clock, and its end time is on the pass's figure.
    let ends =
        Ends { day: Day::Weekday(jiff::civil::Weekday::Saturday), time: TimeOfDay { hour: 3, minute: 14, second: 0 } };
    let hardware = Hardware::default();
    let frame = |at: u64| Frame {
        snapshot: &snapshot,
        net: Some(net()),
        net_err: None,
        now: START + Duration::from_secs(at),
        time: Some(at_one_twenty_seven((at % 60) as u8, ends)),
        hardware: &hardware,
    };
    // The first frames size the scratch buffer and rasterize each digit the clocks show.
    for at in 500..560 {
        dash.render(&frame(at));
    }
    let before = bench::allocations().0;
    dash.render(&frame(560));
    let allocated = bench::allocations().0 - before;
    check!(!dash.output().1.is_empty());
    check!(allocated == 0);
}

/// A run with a goal of four passes, one done, waiting in the second of bit fade's four waits.
fn with_goal() -> SweepStatus {
    let outlook = Outlook {
        goal: Goal { passes: NonZeroU64::new(4), time: None, stop_on_fault: false },
        settled: 1,
        rate: Some(8_000_000_000),
        position: Position::Wait { nth: 2, waits: 4, left: Duration::from_secs(192) },
        test_left: Some(Duration::from_secs(312)),
        test_progress: bp(4512),
        pass: Some(Eta { remaining: Duration::from_mins(10), confidence: Confidence::Estimated }),
        pass_progress: bp(7234),
        cycle: Some(Duration::from_mins(40)),
        cycle_left: Some(Eta { remaining: Duration::from_mins(15), confidence: Confidence::Estimated }),
        cycle_progress: Some(bp(6250)),
        known: (11, 11),
        ..running()
    };
    let mut map = crate::testing::sample_map(0);
    map.cells
        .iter_mut()
        .filter(|c| matches!(c, CellState::Pending | CellState::Active))
        .for_each(|c| *c = CellState::Holding);
    SweepStatus { test: TestKind::BitFade.name(), position: (11, 11), outlook, map, ..status() }
}

#[test]
fn the_overall_bar_stays_empty_without_a_goal_and_fills_against_one() {
    let (history, faults) = (history(), VecDeque::new());
    let filled = |status: SweepStatus| {
        let mut dash = Dashboard::new(1280, 800).expect("dashboard");
        render(&mut dash, status, &history, &faults, 500);
        let l = dash.layout;
        let wanted = [theme::FILL.b, theme::FILL.g, theme::FILL.r, 255];
        let width = l.pass.w as usize;
        (l.pass.y..l.pass.y + l.row_h).any(|y| {
            let row = (y as usize * 1280 + l.pass.x as usize) * 4;
            dash.canvas.bgra()[row..row + width * 4].as_chunks::<4>().0.contains(&wanted)
        })
    };
    check!(!filled(status()), "nothing to measure against, so nothing to fill");
    check!(filled(with_goal()));
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
    let places = [l.errors.body, l.tabs, l.clock, l.log, l.map.body];
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

#[rstest]
fn every_size_renders(
    #[values((768, 576), (1024, 768), (1280, 800), (1920, 1080))] size: (u32, u32),
    #[values(false, true)] faulted: bool,
) {
    let (w, h) = size;
    let mut dash = Dashboard::new(w, h).expect("dashboard");
    let (history, mut faults) = (history(), VecDeque::new());
    let mut status = status();
    if faulted {
        faulty(&mut status, &mut faults);
    }
    check!(render(&mut dash, status, &history, &faults, 523) == [dash.canvas.bounds()]);
}

/// A laptop like the X1 Carbon: a Kaby Lake R with 16 GiB of LPDDR3 in two interleaved channels, running at
/// 3.4 GHz and 58 C.
fn laptop() -> Hardware {
    use ferrite_efi::sensors::{Identity, TempSource};
    use ferrite_hal::PhysAddr;
    use ferrite_power::cpuid::{Signature, Vendor};

    use crate::smbios::{
        DeviceRange, ErrorCorrection, FormFactor, Handle, MegaTransfers, Memory, MemoryArray, MemoryDevice, MemoryKind,
        Size,
    };

    let cpu = Identity {
        vendor: Vendor::Intel,
        signature: Signature { family: 6, model: 0x8e, stepping: 10 },
        brand: "Intel(R) Core(TM) i7-8650U CPU @ 1.90GHz".into(),
    };
    let module = |handle: u16, locator: &str| MemoryDevice {
        handle: Handle(handle),
        array: Handle(1),
        locator: Some(locator.into()),
        bank: Some("BANK 0".into()),
        size: Size::Bytes(NonZeroU64::new(8 << 30).unwrap()),
        kind: MemoryKind::Lpddr3,
        form_factor: FormFactor::Chip,
        speed: MegaTransfers::new(2133),
        configured_speed: MegaTransfers::new(2133),
        manufacturer: Some("Samsung".into()),
        part_number: Some("K4EBE304EB-EGCF".into()),
        ranks: Some(1),
        data_width: Some(64),
        total_width: Some(64),
    };
    let mut memory = Memory {
        devices: vec![module(0x1100, "ChannelA-DIMM0"), module(0x1101, "ChannelB-DIMM0")],
        ..Memory::default()
    };
    memory.arrays.push(MemoryArray { handle: Handle(1), system: true, correction: ErrorCorrection::None, slots: 2 });
    for device in [0x1100, 0x1101] {
        memory.device_ranges.push(DeviceRange {
            device: Handle(device),
            array_range: Handle(0x1300),
            extent: crate::memmap::Extent::new(PhysAddr::new(0), 16 << 30),
            position: Some(0),
            depth: Some(0),
        });
    }
    let hardware = Hardware::new(Facts::new(cpu, memory));
    hardware.set_vitals(Vitals {
        mhz: Some(Mhz(3400)),
        measured: true,
        celsius: Some(58),
        source: Some(TempSource::IntelPackage),
    });
    hardware
}

#[test]
fn the_clock_and_temperature_redraw_the_system_section_only_when_a_shown_figure_changes() {
    let mut dash = Dashboard::new(1280, 800).expect("dashboard");
    let (history, faults, finished) = (history(), VecDeque::new(), finished_tests());
    let snapshot = Snapshot { status: status(), history: &history, faults: &faults, stats: &finished };
    let hardware = laptop();
    let draw = |dash: &mut Dashboard| {
        let now = START + Duration::from_secs(500);
        dash.render(&Frame {
            snapshot: &snapshot,
            net: Some(net()),
            net_err: None,
            now,
            time: None,
            hardware: &hardware,
        });
        dash.output().1.to_vec()
    };
    draw(&mut dash);
    check!(draw(&mut dash).is_empty());
    let shown = hardware.vitals();

    // The same figures from a reading that rounds alike.
    hardware.set_vitals(Vitals::of(ferrite_efi::sensors::Reading {
        frequency: Some(ferrite_efi::sensors::Frequency { mhz: Mhz(3_398), measured: true }),
        temperature: Some(ferrite_efi::sensors::Temperature {
            celsius: 58,
            source: ferrite_efi::sensors::TempSource::IntelPackage,
        }),
    }));
    check!(hardware.vitals() == shown);
    check!(draw(&mut dash).is_empty());

    let l = dash.layout;
    hardware.set_vitals(Vitals { celsius: Some(59), ..shown });
    let rects = draw(&mut dash);
    check!(!rects.is_empty() && rects.iter().all(|r| l.system.body.intersect(*r) == *r), "{rects:?}");
    hardware.set_vitals(Vitals { mhz: Some(Mhz(3410)), ..shown });
    let rects = draw(&mut dash);
    check!(!rects.is_empty() && rects.iter().all(|r| l.system.body.intersect(*r) == *r), "{rects:?}");
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
    for (w, h) in [(1920, 1080), (1024, 768), (768, 576)] {
        for scene in ["running", "fault", "no-network", "goal"] {
            let mut dash = Dashboard::new(w, h).expect("dashboard");
            let (history, mut faults) = (history(), VecDeque::new());
            let mut status = if scene == "goal" { with_goal() } else { status() };
            if scene == "fault" {
                faulty(&mut status, &mut faults);
            }
            let finished = finished_tests();
            let snapshot = Snapshot { status, history: &history, faults: &faults, stats: &finished };
            let (net, net_err) = if scene == "no-network" { (None, no_network) } else { (Some(net()), None) };
            let at = START + Duration::from_secs(523);
            let ends = Ends {
                day: Day::Weekday(jiff::civil::Weekday::Saturday),
                time: TimeOfDay { hour: 3, minute: 14, second: 0 },
            };
            let time = Some(at_one_twenty_seven(43, ends));
            let hardware = laptop();
            dash.render(&Frame { snapshot: &snapshot, net, net_err, now: at, time, hardware: &hardware });
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
    let snapshot = Snapshot { status: status(), history: &history, faults: &faults, stats: &TestStats::default() };
    let hardware = Hardware::default();
    let frame = |net: Option<NetStatus>, net_err| Frame {
        snapshot: &snapshot,
        net,
        net_err,
        now: START,
        time: None,
        hardware: &hardware,
    };

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

fn at_one_twenty_seven(second: u8, ends: Ends) -> TimeLine {
    let time = TimeOfDay { hour: 1, minute: 27, second };
    TimeLine { now: time, format: ClockFormat::Twelve, overall: Some(ends), pass: Some(ends) }
}

#[test]
fn the_time_of_day_and_the_end_time_redraw_only_the_title_bar() {
    let ends = |hour| Ends { day: Day::Today, time: TimeOfDay { hour, minute: 14, second: 0 } };
    let mut dash = Dashboard::new(1280, 800).expect("dashboard");
    let (history, faults, finished) = (history(), VecDeque::new(), finished_tests());
    let snapshot = Snapshot { status: status(), history: &history, faults: &faults, stats: &finished };
    let hardware = Hardware::default();
    let draw = |dash: &mut Dashboard, time: TimeLine| {
        let now = START + Duration::from_secs(500);
        dash.render(&Frame {
            snapshot: &snapshot,
            net: Some(net()),
            net_err: None,
            now,
            time: Some(time),
            hardware: &hardware,
        });
        dash.output().1.to_vec()
    };
    draw(&mut dash, at_one_twenty_seven(43, ends(3)));
    check!(draw(&mut dash, at_one_twenty_seven(43, ends(3))).is_empty());

    let l = dash.layout;
    let rects = draw(&mut dash, at_one_twenty_seven(44, ends(3)));
    check!(!rects.is_empty() && rects.iter().all(|r| l.clock.intersect(*r) == *r), "a new second: {rects:?}");

    let rects = draw(&mut dash, at_one_twenty_seven(44, ends(4)));
    check!(!rects.is_empty() && rects.iter().all(|r| l.clock.intersect(*r) == *r), "a new end time: {rects:?}");
}
