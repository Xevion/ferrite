use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::num::NonZeroU64;
use core::time::Duration;

use assert2::{assert, check};
use ferrite_config::{ClockFormat, zone_named};
use ferrite_efi::sensors::{Identity, TempSource};
use ferrite_hal::{Instant, PhysAddr, UnixTime, WallClock};
use ferrite_mem::analysis::Classification;
use ferrite_mem::suite::{Mode, Preset};
use ferrite_mem::{CpuAddr, Fault};
use ferrite_power::cpuid::{Signature, Vendor};
use rstest::rstest;

use super::*;
use crate::goal::Goal;
use crate::hardware::{Facts, Vitals};
use crate::machine::{Machine, Source};
use crate::memmap::Extent;
use crate::pace::{Outlook, TestStats};
use crate::report::{CpuInfo, History, PassRecord};
use crate::smbios::{
    DeviceRange, ErrorCorrection, FormFactor, Handle, MegaTransfers, Memory, MemoryArray, MemoryDevice, MemoryKind,
    Size,
};
use crate::sweep::{Coverage, SweepStatus};

const fn at(secs: u64) -> Instant {
    Instant::from_epoch(Duration::from_secs(secs))
}

/// The run began at 04:20:00 in Chicago on 14 November 2026 and is reported at 05:23:07.
const RUN_START: u64 = 1_200;
const NOW: u64 = 4_987;
/// 10:00:00 UTC that day, which the clock reads at the monotonic epoch.
const EPOCH_UTC: u64 = 1_794_650_400;

fn wall(synced: bool) -> Wall {
    let utc = UnixTime::from_epoch(Duration::from_secs(EPOCH_UTC));
    let clock = if synced { WallClock::from_fix(at(0), utc) } else { WallClock::from_rtc(at(0), utc) };
    Wall::new(Some(clock), zone_named("America/Chicago").expect("a bundled zone"), ClockFormat::Twelve)
}

fn module(handle: u16, locator: &str, start_gib: u64) -> (MemoryDevice, DeviceRange) {
    let device = MemoryDevice {
        handle: Handle(handle),
        array: Handle(1),
        locator: Some(locator.into()),
        bank: None,
        size: NonZeroU64::new(8 << 30).map_or(Size::Empty, Size::Bytes),
        kind: MemoryKind::Ddr4,
        form_factor: FormFactor::Dimm,
        speed: MegaTransfers::new(3200),
        configured_speed: MegaTransfers::new(2666),
        manufacturer: Some("Samsung".into()),
        part_number: Some("M471A1K43CB1-CTD".into()),
        ranks: Some(1),
        data_width: Some(64),
        total_width: Some(64),
    };
    let range = DeviceRange {
        device: Handle(handle),
        array_range: Handle(0x30),
        extent: Extent::new(PhysAddr::new(start_gib << 30), 8 << 30),
        position: Some(0),
        depth: Some(0),
    };
    (device, range)
}

/// A laptop with two modules that each have a range of their own, and its clock and temperature.
fn hardware(ranges: bool) -> Hardware {
    let cpu = Identity {
        vendor: Vendor::Intel,
        signature: Signature { family: 6, model: 0x8e, stepping: 10 },
        brand: "Intel(R) Core(TM) i7-8650U CPU @ 1.90GHz".into(),
    };
    let ((a, a_range), (b, b_range)) = (module(1, "ChannelA-DIMM0", 0), module(2, "ChannelB-DIMM0", 8));
    let mut memory = Memory { devices: vec![a, b], ..Memory::default() };
    if ranges {
        memory.device_ranges = vec![a_range, b_range];
    }
    memory.arrays.push(MemoryArray {
        handle: Handle(1),
        system: true,
        correction: ErrorCorrection::SingleBitEcc,
        slots: 2,
    });
    let hardware = Hardware::new(Facts::new(cpu, memory));
    hardware.set_machine(Machine { id: 0xfeed_beef_0123_4567, source: Source::SmbiosUuid });
    let reading = |celsius| Vitals {
        mhz: Some(1900),
        measured: true,
        celsius: Some(celsius),
        source: Some(TempSource::IntelPackage),
    };
    for celsius in [61, 74, 63] {
        hardware.set_vitals(reading(celsius));
    }
    hardware
}

fn rec(pass: u64, test: &'static str, cpus: usize, took_ms: u64, rate_mb: u64, faults: u64) -> TestRecord {
    TestRecord { pass, test, cpus, took: Duration::from_millis(took_ms), work_bytes: took_ms * rate_mb * 1000, faults }
}

const TESTS: [&str; 3] = ["address, walking ones", "solid bits", "moving inversions, 1s and 0s"];

/// A pass of the three tests; `slow` is how many times slower than the usual 2.3 s each runs.
fn pass_of(pass: u64, faults_in_last: u64) -> [TestRecord; 3] {
    [
        rec(pass, TESTS[0], 1, 2_310, 12_000, 0),
        rec(pass, TESTS[1], 8, 41_500, 9_000, 0),
        rec(pass, TESTS[2], 8, 125_800, 6_000, faults_in_last),
    ]
}

/// The reserve is a smaller memory, so each test takes less.
fn reserve_of(pass: u64) -> [TestRecord; 3] {
    [
        rec(pass, TESTS[0], 1, 310, 11_000, 0),
        rec(pass, TESTS[1], 8, 4_500, 8_000, 0),
        rec(pass, TESTS[2], 8, 12_800, 6_000, 0),
    ]
}

fn cpus() -> Vec<CpuInfo> {
    (0..8)
        .map(|n| CpuInfo { number: n, package: 0, core: (n / 2) as u32, thread: (n % 2) as u32, bsp: n == 0 })
        .collect()
}

/// What a report is made from.
struct Run {
    status: SweepStatus,
    history: History,
    faults: VecDeque<FaultRecord>,
    stats: TestStats,
    hardware: Hardware,
    wall: Wall,
}

impl Run {
    fn status(settled: u64, faults: u64, goal: Goal) -> SweepStatus {
        SweepStatus {
            bytes: 15_900 << 20,
            test: TESTS[1],
            position: (2, 3),
            test_percent: 40,
            percent: 60,
            passes: settled,
            tier: "avx2",
            mode: Mode::Split,
            cpus: 8,
            faults,
            first_fault: None,
            analysis: if faults == 0 { Classification::Clean } else { Classification::Mixed { stuck: 1 << 3 } },
            preset: Preset::Standard,
            paused: false,
            coverage: Coverage { tested: 15_900 << 20, free: 16_000 << 20 },
            outlook: Outlook {
                goal,
                settled,
                pass_median: Some(Duration::from_mins(30)),
                cycle: Some(Duration::from_mins(31)),
                pace_percent: Some(98),
                bandwidth: Some(18_400_000_000),
                ..Outlook::NONE
            },
            map: crate::testing::sample_map(40),
        }
    }

    fn history() -> History {
        let mut history = History::default();
        history.run_started = Some(at(RUN_START));
        history.cpus = cpus();
        history
    }

    fn render(&self, why: Why) -> String {
        let snapshot =
            Snapshot { status: self.status, history: &self.history, faults: &self.faults, stats: &self.stats };
        Report { snapshot: &snapshot, hardware: &self.hardware, wall: &self.wall, now: at(NOW), why, version: "0.1.0" }
            .render()
    }
}

/// Two passes over three tests with their reserve rotations, to a goal of two passes.
fn passing() -> Run {
    let goal = Goal { passes: NonZeroU64::new(2), ..Goal::UNLIMITED };
    let mut history = Run::history();
    for pass in 0..2 {
        history.tests.extend(pass_of(pass, 0));
        history.reserve.extend(reserve_of(pass));
        history.passes.push_back(PassRecord {
            pass,
            took: Duration::from_millis(1_800_000 + pass * 20_000),
            faults: 0,
        });
    }
    Run {
        status: Run::status(2, 0, goal),
        history,
        faults: VecDeque::new(),
        stats: TestStats::default(),
        hardware: hardware(true),
        wall: wall(true),
    }
}

fn fault(addr: usize, flipped: u64) -> FaultRecord {
    let expected = 0xaaaa_aaaa_5555_5555;
    FaultRecord {
        fault: Fault { addr: CpuAddr::of(addr as *const u64), expected, actual: expected ^ flipped },
        test: TESTS[2],
        cpu: 2,
        pass: 1,
    }
}

/// A run asked for its report with a fault count past what is kept in full, in the third pass, on a
/// clock the network has not corrected.
fn failing() -> Run {
    let mut history = Run::history();
    history.tests.extend(pass_of(0, 0));
    history.reserve.extend(reserve_of(0));
    history.passes.push_back(PassRecord { pass: 0, took: Duration::from_mins(30), faults: 0 });
    history.tests.extend(pass_of(1, 130));
    history.passes.push_back(PassRecord { pass: 1, took: Duration::from_secs(1_821), faults: 130 });
    history.tests.push_back(rec(2, TESTS[0], 1, 2_300, 12_000, 0));
    Run {
        status: Run::status(1, 130, Goal::UNLIMITED),
        history,
        faults: VecDeque::from([
            fault(0x1_2340_0000, 1 << 3),
            fault(0x2_0000_1000, 1 << 3 | 1 << 41),
            fault(0x2_0000_2000, 0x3_ffff),
        ]),
        stats: TestStats::default(),
        hardware: hardware(true),
        wall: wall(false),
    }
}

/// Written when the goal of two passes is met.
const PASSING: &str = "\
FERRITE MEMORY TEST REPORT

Verdict       PASS (no faults)
Version       ferrite 0.1.0
Written       2026-11-14 05:23:07 America/Chicago
Reason        goal met: 2 passes done

MACHINE
-------
Machine id    feedbeef01234567 (from the SMBIOS system UUID)
CPU           Intel(R) Core(TM) i7-8650U CPU @ 1.90GHz
Signature     GenuineIntel family 6 model 0x8e stepping 10
Memory        2 x 8 GiB DDR4-2666
ECC           single-bit ECC (the firmware's claim, not verified)
Layout        2 modules, separate ranges
Modules       ChannelA-DIMM0: 8 GiB DDR4 3200 MT/s (running 2666 MT/s), part M471A1K43CB1-CTD by Samsung
              ChannelB-DIMM0: 8 GiB DDR4 3200 MT/s (running 2666 MT/s), part M471A1K43CB1-CTD by Samsung
Clock         1900 MHz (measured)
Temperature   63 C at the last reading, 74 C the highest of the run
              read from the Intel package thermal status

RUN
---
Preset        standard
Kernels       avx2
Cores         8 logical processors on 4 cores
Goal          2 passes; met (2 passes done)
Started       2026-11-14 04:20:00 America/Chicago
Ended         2026-11-14 05:23:07 America/Chicago
Elapsed       01:03:07
Passes        2 (a pass counts once its reserve rotation has run)
Memory        15900 MiB under test
Coverage      15900 of 16000 MiB free (99%)
Median pass   30:00
Predicted     31:00 for a pass with its reserve rotation
Against saved 98% of the time the same tests took before
Bandwidth     18.4 GB/s measured before the first test

PASSES
------

Pass 1: 30:00, 0 faults
  test                               cpus      time       rate  faults
  address, walking ones                 1    2.31 s  12.0 GB/s       0
  solid bits                            8    41.5 s  9.00 GB/s       0
  moving inversions, 1s and 0s          8      2:06  6.00 GB/s       0
  Reserve rotation after pass 1: 17.6 s, 0 faults
  test                               cpus      time       rate  faults
  address, walking ones                 1    0.31 s  11.0 GB/s       0
  solid bits                            8    4.50 s  8.00 GB/s       0
  moving inversions, 1s and 0s          8    12.8 s  6.00 GB/s       0

Pass 2: 30:20, 0 faults
  test                               cpus      time       rate  faults
  address, walking ones                 1    2.31 s  12.0 GB/s       0
  solid bits                            8    41.5 s  9.00 GB/s       0
  moving inversions, 1s and 0s          8      2:06  6.00 GB/s       0
  Reserve rotation after pass 2: 17.6 s, 0 faults
  test                               cpus      time       rate  faults
  address, walking ones                 1    0.31 s  11.0 GB/s       0
  solid bits                            8    4.50 s  8.00 GB/s       0
  moving inversions, 1s and 0s          8    12.8 s  6.00 GB/s       0

FAULTS
------
Total         0 words read back wrong
Analysis      no faults
";

/// Asked for with F10 part way through the third pass.
const FAILING: &str = "\
FERRITE MEMORY TEST REPORT

Verdict       FAIL (130 faults)
Version       ferrite 0.1.0
Written       2026-11-14 05:23:07 America/Chicago (clock not synced)
Reason        requested (F10 or the status page); the run was still going

MACHINE
-------
Machine id    feedbeef01234567 (from the SMBIOS system UUID)
CPU           Intel(R) Core(TM) i7-8650U CPU @ 1.90GHz
Signature     GenuineIntel family 6 model 0x8e stepping 10
Memory        2 x 8 GiB DDR4-2666
ECC           single-bit ECC (the firmware's claim, not verified)
Layout        2 modules, separate ranges
Modules       ChannelA-DIMM0: 8 GiB DDR4 3200 MT/s (running 2666 MT/s), part M471A1K43CB1-CTD by Samsung
              ChannelB-DIMM0: 8 GiB DDR4 3200 MT/s (running 2666 MT/s), part M471A1K43CB1-CTD by Samsung
Clock         1900 MHz (measured)
Temperature   63 C at the last reading, 74 C the highest of the run
              read from the Intel package thermal status

RUN
---
Preset        standard
Kernels       avx2
Cores         8 logical processors on 4 cores
Goal          until stopped
Started       2026-11-14 04:20:00 America/Chicago (clock not synced)
Ended         still running
Elapsed       01:03:07
Passes        1 (a pass counts once its reserve rotation has run)
Memory        15900 MiB under test
Coverage      15900 of 16000 MiB free (99%)
Median pass   30:00
Predicted     31:00 for a pass with its reserve rotation
Against saved 98% of the time the same tests took before
Bandwidth     18.4 GB/s measured before the first test

PASSES
------

Pass 1: 30:00, 0 faults
  test                               cpus      time       rate  faults
  address, walking ones                 1    2.31 s  12.0 GB/s       0
  solid bits                            8    41.5 s  9.00 GB/s       0
  moving inversions, 1s and 0s          8      2:06  6.00 GB/s       0
  Reserve rotation after pass 1: 17.6 s, 0 faults
  test                               cpus      time       rate  faults
  address, walking ones                 1    0.31 s  11.0 GB/s       0
  solid bits                            8    4.50 s  8.00 GB/s       0
  moving inversions, 1s and 0s          8    12.8 s  6.00 GB/s       0

Pass 2: 30:21, 130 faults
  test                               cpus      time       rate  faults
  address, walking ones                 1    2.31 s  12.0 GB/s       0
  solid bits                            8    41.5 s  9.00 GB/s       0
  moving inversions, 1s and 0s          8      2:06  6.00 GB/s     130

Pass 3: under way, 1 test finished, 0 faults so far
  test                               cpus      time       rate  faults
  address, walking ones                 1    2.30 s  12.0 GB/s       0

FAULTS
------
Total         130 words read back wrong
Analysis      mixed (stuck 0x8, others vary)
Listed        the newest 3 of 130, oldest first

  1. 0x0000000123400000  in ChannelA-DIMM0
     expected 0xaaaaaaaa55555555  read 0xaaaaaaaa5555555d  xor 0x0000000000000008
     bits 3
     found by moving inversions, 1s and 0s on cpu 2 in pass 2

  2. 0x0000000200001000  in ChannelB-DIMM0
     expected 0xaaaaaaaa55555555  read 0xaaaaa8aa5555555d  xor 0x0000020000000008
     bits 3, 41
     found by moving inversions, 1s and 0s on cpu 2 in pass 2

  3. 0x0000000200002000  in ChannelB-DIMM0
     expected 0xaaaaaaaa55555555  read 0xaaaaaaaa5556aaaa  xor 0x000000000003ffff
     bits 0, 1, 2, 3, 4, 5, 6, 7, ...
     found by moving inversions, 1s and 0s on cpu 2 in pass 2
";

#[test]
fn a_passing_run_reads_as_expected() {
    let text = passing().render(Why::GoalMet(Met::Passes(NonZeroU64::new(2).unwrap())));
    check!(text == PASSING, "{text}");
}

#[test]
fn a_failing_run_lists_its_faults_with_their_modules_and_says_what_was_left_out() {
    let text = failing().render(Why::Requested);
    check!(text == FAILING, "{text}");
}

#[rstest]
#[case::nothing_finished(0, 0, Verdict::Incomplete, "INCOMPLETE (no full pass finished)")]
#[case::a_pass_without_faults(0, 1, Verdict::Pass, "PASS (no faults)")]
#[case::many_passes(0, 40, Verdict::Pass, "PASS (no faults)")]
#[case::one_fault(1, 5, Verdict::Fail { faults: 1 }, "FAIL (1 fault)")]
#[case::a_fault_before_any_pass_finished(3, 0, Verdict::Fail { faults: 3 }, "FAIL (3 faults)")]
fn the_verdict_is_a_failure_for_any_fault_and_a_pass_only_once_a_whole_pass_has_finished(
    #[case] faults: u64,
    #[case] settled: u64,
    #[case] verdict: Verdict,
    #[case] text: &str,
) {
    check!(Verdict::of(faults, settled) == verdict);
    check!(alloc::format!("{verdict}") == text);
}

#[test]
fn a_run_that_has_finished_nothing_says_so_and_is_incomplete() {
    let run =
        Run { status: Run::status(0, 0, Goal::UNLIMITED), history: History::default(), wall: wall(true), ..passing() };
    let text = run.render(Why::Quit);
    check!(text.contains("Verdict       INCOMPLETE (no full pass finished)\n"));
    check!(text.contains("Started       not yet\n"));
    check!(text.contains("No test has finished yet.\n"));
    check!(text.contains("Reason        the run was quit\n"));
}

#[test]
fn a_clock_without_a_source_gives_no_time_of_day() {
    let run = Run { wall: Wall::new(None, zone_named("UTC").unwrap(), ClockFormat::Twelve), ..passing() };
    let text = run.render(Why::Quit);
    check!(text.contains("Written       clock not synced\n"));
    check!(text.contains("Started       clock not synced\n"));
}

#[test]
fn what_the_firmware_spells_outside_ascii_is_shown_as_a_question_mark() {
    let mut run = passing();
    run.hardware.facts.cpu.brand = "Intel\u{ae} Core\u{2122} i7".into();
    run.hardware.facts.memory.devices[0].part_number = Some("M4\u{e9}1".into());
    let text = run.render(Why::Quit);
    assert!(text.is_ascii());
    check!(text.contains("CPU           Intel? Core? i7\n"));
    check!(text.contains("part M4?1 by Samsung"));
}

#[rstest]
#[case::first_byte(0, Some("ChannelA-DIMM0"))]
#[case::last_byte_of_the_first((8 << 30) - 1, Some("ChannelA-DIMM0"))]
#[case::first_byte_of_the_second(8 << 30, Some("ChannelB-DIMM0"))]
#[case::inside_the_second((8 << 30) + 4096, Some("ChannelB-DIMM0"))]
#[case::end_is_exclusive(16 << 30, None)]
#[case::in_a_hole_below_the_modules(0x1_0000_0000_0000, None)]
fn a_fault_is_placed_in_the_band_that_holds_it(#[case] address: u64, #[case] module: Option<&str>) {
    let hardware = hardware(true);
    check!(module_of(&hardware.facts.layout, address) == module);
}

#[test]
fn interleaved_or_unmapped_modules_cannot_place_a_fault() {
    // Without ranges the layout is unknown; with the same range for both the controller interleaves.
    check!(module_of(&hardware(false).facts.layout, 0x1000).is_none());
    check!(module_of(&Layout::Interleaved { ways: 2 }, 0x1000).is_none());
}

#[rstest]
#[case(0b1, "0")]
#[case(1 << 3 | 1 << 41, "3, 41")]
#[case(0xff, "0, 1, 2, 3, 4, 5, 6, 7")]
#[case(0x1ff, "0, 1, 2, 3, 4, 5, 6, 7, ...")]
fn flipped_bits_are_listed_up_to_eight(#[case] mask: u64, #[case] text: &str) {
    check!(alloc::format!("{}", Bits(mask)) == text);
}
