use core::time::Duration;
use std::string::ToString;

use arbitrary_int::u7;
use assert2::{assert, check};
use ferrite_hal::Instant;
use rstest::rstest;

use crate::amd::{self, PStateDef};
use crate::control::{Bound, Epp, Interface, Request};
use crate::cpuid::{Feature, Signature, Vendor, read_features};
use crate::discover::{Platform, Refusal};
use crate::fake::{Cpuid, Msrs, vendor};
use crate::intel::{self, models::Model};
use crate::limits::{Layout, Limit};
use crate::rapl::{self, Counter, Domain, EnergyUnit, Flavor, PackageLimits, PowerUnit, Units};
use crate::telemetry::Meter;
use crate::units::{Mhz, MicroJoules, MilliWatts, effective_mhz};

/// The power-unit value every Intel client part since Sandy Bridge reports: 1/8 W, 2^-14 J, 2^-10 s.
const CLIENT_UNITS: u64 = 0x000a_0e03;

fn intel_cpuid(eax1: u32, leaf6_eax: u32) -> Cpuid {
    let [b, c, d] = vendor(b"GenuineIntel");
    Cpuid::with(&[
        (0, [0x20, b, c, d]),
        (1, [eax1, 0, 1 << 7, 0]),
        (6, [leaf6_eax, 0, 0b1001, 0]),
        (0x8000_0000, [0x8000_0008, 0, 0, 0]),
    ])
}

/// An i5-8350U as the X1 Carbon reports it: HWP with EPP, RAPL, and HWP still off from the firmware.
fn kaby_lake_r() -> (Cpuid, Msrs) {
    let cpuid = intel_cpuid(0x0008_06ea, 0b0111_1111_0111);
    let msrs = Msrs::with(&[
        (0xe8, 1),
        (0x198, 0x1000),
        (0x1a0, 0x0085_0089),
        (0x770, 0),
        (0x771, 0x0104_1124),
        (0x774, 0x8000_ff01),
        (0x606, CLIENT_UNITS),
        (0x610, 0x0042_8160_00dc_8078),
        (0x611, 0x1234),
        (0x613, 0),
        (0x614, 0x78),
        (0x639, 0x55),
        (0x64d, 0),
        (0x64e, 7),
        (0x64f, 0),
    ]);
    (cpuid, msrs)
}

#[test]
fn features_come_from_leaves_inside_their_range_only() {
    let cpuid = intel_cpuid(0x0008_06ea, 1 << 7 | 1 << 10);
    let features = read_features(&cpuid);
    check!(features.has(Feature::Hwp));
    check!(features.has(Feature::HwpEpp));
    check!(features.has(Feature::Eist));
    check!(features.has(Feature::Epb));
    check!(!features.has(Feature::Hybrid), "leaf 7 is absent and reads as zero");
    check!(features.to_string() == "eist hwp hwp_epp aperf_mperf epb");
}

#[test]
fn rapl_units_scale_counts_watts_and_time() {
    let units = Units::new(Flavor::Core, PowerUnit::new_with_raw_value(CLIENT_UNITS));
    check!(units.energy(Domain::Package).to_micro(16_384) == MicroJoules(1_000_000));
    check!(units.power(120) == MilliWatts(15_000));
    check!(units.window(u7::new(0x6e)) == Duration::from_secs(28), "2^14 * (1 + 3/4) / 1024 s");
    let server = Units::new(Flavor::HaswellServer, PowerUnit::new_with_raw_value(CLIENT_UNITS));
    check!(server.energy(Domain::Dram) == EnergyUnit::NanoJoules(15_300));
    let atom = Units::new(Flavor::Silvermont, PowerUnit::new_with_raw_value(0x0005_0505));
    check!(atom.energy(Domain::Package).to_micro(3) == MicroJoules(96));
    check!(atom.power(2) == MilliWatts(64));
}

#[test]
fn package_limits_decode_both_terms_and_the_lock() {
    let units = Units::new(Flavor::Core, PowerUnit::new_with_raw_value(CLIENT_UNITS));
    let limits = PackageLimits::decode(
        units,
        rapl::PackagePowerLimit::new_with_raw_value(0x8042_8160_00dc_8078),
        Some(rapl::PowerInfo::new_with_raw_value(0x78)),
    );
    check!(limits.pl1.power == MilliWatts(15_000));
    check!(limits.pl1.enabled);
    check!(limits.pl1.window == Duration::from_secs(28));
    check!(limits.pl2.power == MilliWatts(44_000));
    check!(limits.locked);
    check!(limits.thermal_design == Some(MilliWatts(15_000)));
}

#[test]
fn an_energy_counter_keeps_counting_across_its_32_bit_wrap() {
    let mut counter = Counter::default();
    check!(counter.update(0xffff_fff0) == 0);
    check!(counter.update(0x10) == 0x20);
    check!(counter.update(0x1_0000_0030) == 0x20, "only the low 32 bits count");
    check!(counter.total() == 0x40);
}

#[rstest]
#[case::zen3_3600(0x19, 1 << 63 | 0x08 << 8 | 0x90, Some(3600))]
#[case::zen5_4200(0x1a, 1 << 63 | 0x348, Some(4200))]
#[case::k10_2600(0x10, 1 << 63 | 0x0a, Some(2600))]
#[case::disabled(0x19, 0x08 << 8 | 0x90, None)]
#[case::zero_divisor(0x17, 1 << 63 | 0x90, None)]
fn a_pstate_definition_gives_its_clock_by_family(#[case] family: u16, #[case] raw: u64, #[case] mhz: Option<u32>) {
    check!(PStateDef::new_with_raw_value(raw).frequency(family) == mhz.map(Mhz));
}

#[rstest]
#[case::skylake(Layout::Skylake, 1 << 1 | 1 << 10 | 1 << 16, &[Limit::Thermal, Limit::PowerPl1], &[Limit::Prochot])]
#[case::skylake_turbo(Layout::Skylake, 1 << 12 | 1 << 28, &[Limit::MultiCoreTurbo], &[Limit::MultiCoreTurbo])]
#[case::haswell(Layout::Haswell, 1 << 9 | 1 << 4, &[Limit::Graphics, Limit::CorePower], &[])]
fn limit_reasons_decode_status_and_log_bits(
    #[case] layout: Layout,
    #[case] raw: u64,
    #[case] active: &[Limit],
    #[case] logged: &[Limit],
) {
    let reasons = layout.decode(raw);
    check!(reasons.active.iter().collect::<std::vec::Vec<_>>() == active);
    check!(reasons.logged.iter().collect::<std::vec::Vec<_>>() == logged);
}

#[test]
fn kaby_lake_r_is_found_with_hwp_rapl_limits_and_pperf() {
    let (cpuid, mut msrs) = kaby_lake_r();
    let platform = Platform::discover(&cpuid, &mut msrs);
    check!(platform.model == Some(Model::KabyLakeL));
    assert!(let Ok(Interface::Hwp { p_core_khz: None, .. }) = platform.control);
    check!(platform.firmware.is_some_and(|f| !f.enabled && f.current == Some(Mhz(1600))));
    let energy = platform.energy.unwrap();
    check!(energy.domains.to_string() == "package cores platform", "no PP1 or DRAM counter on this machine");
    check!(platform.package_limits.unwrap().pl2.power == MilliWatts(44_000));
    check!(platform.limit_reasons == Some(Layout::Skylake));
    check!(platform.pperf);
    check!(msrs.writes.is_empty(), "discovery only reads");
}

#[test]
fn hwp_max_enables_hwp_then_pins_the_highest_level_and_restores_the_old_request() {
    let (cpuid, mut msrs) = kaby_lake_r();
    let interface = Platform::discover(&cpuid, &mut msrs).control.unwrap();
    let applied = interface.apply(Request::MAX, &cpuid, &mut msrs).unwrap();
    check!(applied.took());
    check!(msrs.writes == [(0x770, 1), (0x774, 0x2424)], "min and max 36, desired 0, EPP 0");
    let levels = applied.levels.unwrap();
    check!(levels.mhz(levels.highest) == Some(Mhz(3600)));
    applied.saved.restore(&mut msrs).unwrap();
    check!(msrs.values[&0x774] == 0x8000_ff01);
    check!(msrs.values[&0x770] == 1, "HWP stays on");
}

#[test]
fn a_clock_ceiling_rounds_down_to_a_level_and_clamps_to_the_core_s_range() {
    let (cpuid, mut msrs) = kaby_lake_r();
    let interface = Platform::discover(&cpuid, &mut msrs).control.unwrap();
    let request = Request { floor: Bound::Efficient, ceiling: Bound::Clock(Mhz(2650)), preference: Epp::BALANCE_POWER };
    interface.apply(request, &cpuid, &mut msrs).unwrap();
    check!(msrs.values[&0x774] == 0xc000_1a04, "min 4 (efficient), max 26, EPP 0xc0");
}

#[test]
fn a_hybrid_p_core_s_level_is_scaled_by_the_model_table() {
    let mut cpuid = intel_cpuid(0x0009_06a3, 1 << 7);
    cpuid.0.insert(7, crate::cpuid::Leaf { edx: 1 << 15, ..Default::default() });
    cpuid.0.insert(0x1a, crate::cpuid::Leaf { eax: 0x40 << 24, ..Default::default() });
    let mut msrs = Msrs::with(&[(0x1b0, 6), (0x770, 1), (0x771, 0x0a2b_3a46), (0x774, 0)]);
    let interface = Platform::discover(&cpuid, &mut msrs).control.unwrap();
    let applied = interface.apply(Request::capped(Bound::Clock(Mhz(3000))), &cpuid, &mut msrs).unwrap();
    check!(applied.levels.unwrap().khz_per_level == Some(78_741));
    check!(msrs.values[&0x774] & 0xff00 == 38 << 8, "3000 MHz / 78.741 MHz per level");
}

#[test]
fn zen_cppc_scales_levels_from_p0_and_caps_by_clock() {
    let [b, c, d] = vendor(b"AuthenticAMD");
    let cpuid = Cpuid::with(&[
        (0, [0x10, b, c, d]),
        (1, [0x00a2_0f10, 0, 0, 0]),
        (0x8000_0000, [0x8000_0008, 0, 0, 0]),
        (0x8000_0007, [0, 0, 0, 1 << 7 | 1 << 9 | 1 << 14]),
        (0x8000_0008, [0, 1 << 27, 0, 0]),
    ]);
    let mut msrs = Msrs::with(&[
        (0xc001_0015, 0),
        (0xc001_0061, 0x20),
        (0xc001_0063, 0),
        (0xc001_0064, 1 << 63 | 0x08 << 8 | 0x94),
        (0xc001_0065, 1 << 63 | 0x0a << 8 | 0x8c),
        (0xc001_0066, 1 << 63 | 0x0c << 8 | 0x6c),
        (0xc001_0299, CLIENT_UNITS),
        (0xc001_029a, 0),
        (0xc001_029b, 0),
        (0xc001_02b0, 0xa6_7c_31_1e),
        (0xc001_02b1, 0),
        (0xc001_02b3, 0x0000_1ea6),
    ]);
    let platform = Platform::discover(&cpuid, &mut msrs);
    assert!(let Ok(Interface::Cppc { nominal: Some(Mhz(3700)) }) = platform.control);
    check!(platform.energy.unwrap().domains.to_string() == "package", "the per-core counter is left out");
    let applied = platform.control.unwrap().apply(Request::capped(Bound::Clock(Mhz(3000))), &cpuid, &mut msrs).unwrap();
    check!(msrs.writes[0] == (amd::CPPC_ENABLE.address(), 1));
    check!(applied.written == 0x0000_1e64, "max 100 = 3000 MHz at 3700/124 per level, min lowest");
}

#[test]
fn amd_pstates_pick_the_fastest_state_under_a_ceiling_and_turn_boost_off() {
    let [b, c, d] = vendor(b"AuthenticAMD");
    let cpuid = Cpuid::with(&[
        (0, [0x10, b, c, d]),
        (1, [0x0080_0f11, 0, 0, 0]),
        (0x8000_0000, [0x8000_0007, 0, 0, 0]),
        (0x8000_0007, [0, 0, 0, 1 << 7 | 1 << 9]),
    ]);
    let mut msrs = Msrs::with(&[
        (0xc001_0015, 0),
        (0xc001_0061, 0x20),
        (0xc001_0062, 0),
        (0xc001_0063, 0),
        (0xc001_0064, 1 << 63 | 0x08 << 8 | 0x88),
        (0xc001_0065, 1 << 63 | 0x0a << 8 | 0x8c),
        (0xc001_0066, 1 << 63 | 0x0c << 8 | 0x6c),
    ]);
    let platform = Platform::discover(&cpuid, &mut msrs);
    let interface = platform.control.unwrap();
    check!(interface.to_string() == "AMD P-states P0 3400 MHz P1 2800 MHz P2 1800 MHz, boost");
    let applied = interface.apply(Request::capped(Bound::Clock(Mhz(3000))), &cpuid, &mut msrs).unwrap();
    check!(msrs.values[&0xc001_0062] == 1);
    check!(msrs.values[&0xc001_0015] == 1 << 25);
    applied.saved.restore(&mut msrs).unwrap();
    check!((msrs.values[&0xc001_0062], msrs.values[&0xc001_0015]) == (0, 0));
}

#[rstest]
#[case::hypervisor(0x0008_06ea, 1 << 31, Refusal::Hypervisor)]
#[case::unknown_model_without_hwp(0x0001_06a5, 1 << 7, Refusal::UnknownModel { family: 6, model: 0x1a })]
fn processors_that_cannot_be_asked_say_why(#[case] eax1: u32, #[case] ecx1: u32, #[case] why: Refusal) {
    let mut cpuid = intel_cpuid(eax1, 0);
    cpuid.0.get_mut(&1).unwrap().ecx = ecx1;
    let platform = Platform::discover(&cpuid, &mut Msrs::default());
    check!(platform.control == Err(why));
}

#[test]
fn the_meter_turns_counter_deltas_into_watts_and_tallies_limits() {
    let (cpuid, mut msrs) = kaby_lake_r();
    let platform = Platform::discover(&cpuid, &mut msrs);
    let mut meter = Meter::new(&platform);
    let start = Instant::from_epoch(Duration::from_secs(10));
    check!(meter.sample(&mut msrs, start).is_none());
    *msrs.values.get_mut(&0x611).unwrap() += 15 * 16_384;
    *msrs.values.get_mut(&0x613).unwrap() += 512;
    msrs.values.insert(0x64f, 1 << 1);
    let sample = meter.sample(&mut msrs, Instant::from_epoch(Duration::from_secs(11))).unwrap();
    check!(sample.power.of(Domain::Package) == Some(MilliWatts(15_000)));
    check!(sample.throttled == Some(Duration::from_millis(500)));
    check!(meter.tally().count(Limit::Thermal) == 1);
    check!(meter.energy(Domain::Package) == Some(MicroJoules(15_000_000)));
}

#[test]
fn the_signature_folds_in_the_extended_fields() {
    // Kaby Lake R: family 6, extended model 8, model 0xe, stepping 10.
    check!(Signature::from_eax(0x0008_06ea) == Signature { family: 6, model: 0x8e, stepping: 10 });
    // Zen 3: base family 0xf and extended family 0xa make family 25; extended model 2, model 1.
    check!(Signature::from_eax(0x00a2_0f10) == Signature { family: 25, model: 0x21, stepping: 0 });
}

#[test]
fn the_vendor_string_is_read_in_the_registers_order() {
    let le = |s: &[u8; 4]| u32::from_le_bytes(*s);
    check!(Vendor::from_registers(le(b"Genu"), le(b"ntel"), le(b"ineI")) == Vendor::Intel);
    check!(Vendor::from_registers(le(b"Auth"), le(b"cAMD"), le(b"enti")) == Vendor::Amd);
    check!(Vendor::from_registers(le(b"Hygo"), le(b"uine"), le(b"nGen")) == Vendor::Amd);
    assert!(let Vendor::Other(_) = Vendor::from_registers(0, 0, 0));
}

#[test]
fn the_digital_readout_counts_down_from_tjmax_and_needs_the_valid_bit() {
    let target = intel::TemperatureTarget::new_with_raw_value(0x0564_0000);
    check!(target.tj_max() == 100);
    check!(target.throttle_celsius() == Some(95), "TCC offset 5");
    check!(intel::ThermStatus::new_with_raw_value(0x802a_0000).celsius(100) == Some(58));
    check!(intel::ThermStatus::new_with_raw_value(0x002a_0000).celsius(100) == None);
}

#[rstest]
#[case::room(200_u32 << 21, Some(25))]
#[case::rounds_to_nearest(201_u32 << 21, Some(25))]
#[case::range_select_lowers_it(0x0008_0000 | (800_u32 << 21), Some(51))]
#[case::below_zero(0x0008_0000 | (160_u32 << 21), Some(-29))]
#[case::nothing_there(u32::MAX, None)]
fn the_amd_tctl_register_is_eighths_of_a_degree_with_a_range_bit(#[case] raw: u32, #[case] celsius: Option<i16>) {
    check!(amd::tctl_celsius(raw) == celsius);
}

#[rstest]
#[case::turbo(1_896_000_000, 3_000_000, 1_000_000, Some(5688))]
#[case::half_speed(1_896_000_000, 500_000, 1_000_000, Some(948))]
#[case::never_ran(1_896_000_000, 5, 0, None)]
#[case::no_progress(1_896_000_000, 0, 1_000_000, None)]
fn the_effective_clock_scales_the_tsc_by_the_counters(
    #[case] tsc_hz: u64,
    #[case] aperf: u64,
    #[case] mperf: u64,
    #[case] mhz: Option<u32>,
) {
    check!(effective_mhz(tsc_hz, aperf, mperf) == mhz.map(Mhz));
}
