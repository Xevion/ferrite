//! Intel's clock and thermal registers.
//!
//! Hardware P-states (HWP) take a range of performance levels in `IA32_HWP_REQUEST` once
//! `IA32_PM_ENABLE` has turned them on, which lasts until reset; its capabilities fault until then.
//! Without HWP the clock is one ratio of the 100 MHz bus in `IA32_PERF_CTL`. An HWP performance
//! level is that ratio too, except on the P-cores of hybrid parts, where [`models::Model`] gives the
//! scale.

use arbitrary_int::{u4, u6, u7, u10};
use bitbybit::bitfield;

use crate::msr::{Msr, register};
use crate::units::Mhz;

pub mod models;

/// The bus clock that ratios multiply on every Intel core since Sandy Bridge.
pub const BUS_MHZ: u32 = 100;

pub const IA32_MPERF: Msr<u64> = Msr::at(0xe7);
pub const IA32_APERF: Msr<u64> = Msr::at(0xe8);
pub const PLATFORM_INFO: Msr<PlatformInfo> = Msr::at(0xce);
pub const IA32_PERF_STATUS: Msr<PerfStatus> = Msr::at(0x198);
pub const IA32_PERF_CTL: Msr<PerfCtl> = Msr::at(0x199);
pub const IA32_THERM_STATUS: Msr<ThermStatus> = Msr::at(0x19c);
pub const IA32_MISC_ENABLE: Msr<MiscEnable> = Msr::at(0x1a0);
pub const TEMPERATURE_TARGET: Msr<TemperatureTarget> = Msr::at(0x1a2);
pub const TURBO_RATIO_LIMIT: Msr<TurboRatioLimit> = Msr::at(0x1ad);
pub const IA32_ENERGY_PERF_BIAS: Msr<EnergyPerfBias> = Msr::at(0x1b0);
pub const IA32_PACKAGE_THERM_STATUS: Msr<ThermStatus> = Msr::at(0x1b1);
/// Productive cycles: counts like APERF but not while the core is stalled. Skylake on.
pub const PPERF: Msr<u64> = Msr::at(0x64e);
pub const IA32_PM_ENABLE: Msr<PmEnable> = Msr::at(0x770);
pub const IA32_HWP_CAPABILITIES: Msr<HwpCapabilities> = Msr::at(0x771);
pub const IA32_HWP_REQUEST_PKG: Msr<HwpRequest> = Msr::at(0x772);
pub const IA32_HWP_REQUEST: Msr<HwpRequest> = Msr::at(0x774);
pub const IA32_HWP_STATUS: Msr<HwpStatus> = Msr::at(0x777);

/// `MSR_PLATFORM_INFO`: the ratios the part is sold at.
#[bitfield(u64, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct PlatformInfo {
    /// The base clock's ratio.
    #[bits(8..=15, r)]
    max_non_turbo_ratio: u8,
    /// The turbo ratio limits are writable.
    #[bit(28, r)]
    programmable_turbo: bool,
    /// The TDP limits are writable.
    #[bit(29, r)]
    programmable_tdp: bool,
    /// The lowest ratio the part runs at efficiently, the floor of what software asks for.
    #[bits(40..=47, r)]
    max_efficiency_ratio: u8,
    /// The lowest ratio the part runs at, on parts that report it.
    #[bits(48..=55, r)]
    min_operating_ratio: u8,
}

/// `IA32_PERF_STATUS`.
#[bitfield(u64, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct PerfStatus {
    /// The ratio the core runs at now.
    #[bits(8..=15, r)]
    current_ratio: u8,
    /// The core voltage in units of 1/8192 V, on Skylake-generation client parts and later.
    #[bits(32..=47, r)]
    voltage: u16,
}

/// `IA32_PERF_CTL`: the ratio software asks for without HWP.
#[bitfield(u64, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct PerfCtl {
    #[bits(8..=15, rw)]
    target_ratio: u8,
    /// Stay out of turbo even when the ratio asks for it.
    #[bit(32, rw)]
    turbo_disengage: bool,
}

/// `IA32_MISC_ENABLE`, the two bits that matter for the clock.
#[bitfield(u64, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct MiscEnable {
    #[bit(16, r)]
    eist: bool,
    /// The firmware has turned turbo off.
    #[bit(38, r)]
    turbo_disable: bool,
}

/// `MSR_TEMPERATURE_TARGET`.
#[bitfield(u64, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct TemperatureTarget {
    #[bits(8..=15, r)]
    fan_offset: u8,
    /// `TjMax`: the temperature the digital readouts count down from.
    #[bits(16..=23, r)]
    tj_max: u8,
    /// How far below `TjMax` the thermal control circuit starts throttling.
    #[bits(24..=29, r)]
    tcc_offset: u6,
}

impl TemperatureTarget {
    /// The temperature at which the processor throttles itself.
    #[must_use]
    pub fn throttle_celsius(self) -> Option<i16> {
        let tj = i16::from(self.tj_max());
        (tj != 0).then(|| tj - i16::from(self.tcc_offset().value()))
    }
}

/// `IA32_THERM_STATUS` and `IA32_PACKAGE_THERM_STATUS`. Each status bit has a log bit beside it
/// that stays set until software writes 0.
#[bitfield(u64, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct ThermStatus {
    /// Throttling for heat now.
    #[bit(0, r)]
    thermal: bool,
    #[bit(1, r)]
    thermal_log: bool,
    /// PROCHOT or FORCEPR asserted now.
    #[bit(2, r)]
    prochot: bool,
    #[bit(3, r)]
    prochot_log: bool,
    /// At the critical temperature now.
    #[bit(4, r)]
    critical: bool,
    #[bit(5, r)]
    critical_log: bool,
    /// Held below the requested clock by a power limit now (with power limit notification).
    #[bit(10, r)]
    power_limit: bool,
    #[bit(11, r)]
    power_limit_log: bool,
    /// Degrees below `TjMax`.
    #[bits(16..=22, r)]
    readout: u7,
    /// The readout's resolution in degrees.
    #[bits(27..=30, r)]
    resolution: u4,
    #[bit(31, r)]
    valid: bool,
}

impl ThermStatus {
    /// Degrees Celsius, good only while the valid bit is set.
    #[must_use]
    pub fn celsius(self, tj_max: u8) -> Option<i16> {
        self.valid().then(|| i16::from(tj_max) - i16::from(self.readout().value()))
    }
}

/// `MSR_TURBO_RATIO_LIMIT` on client parts: byte `n` is the highest ratio with `n + 1` cores active.
#[bitfield(u64, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct TurboRatioLimit {
    #[bits(0..=7, r)]
    one_core: u8,
}

impl TurboRatioLimit {
    /// The highest ratio with `active` cores busy, for 1 to 8; `None` past the register or when the
    /// byte is unset.
    #[must_use]
    pub fn with_active(self, active: u8) -> Option<u8> {
        let byte = active.checked_sub(1).filter(|&i| i < 8)?;
        Some((self.raw_value() >> (8 * u32::from(byte))) as u8).filter(|&r| r != 0)
    }
}

/// `IA32_ENERGY_PERF_BIAS`: 0 performance to 15 power saving, the hint processors without HWP
/// EPP take.
#[bitfield(u64, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct EnergyPerfBias {
    #[bits(0..=3, rw)]
    bias: u4,
}

/// `IA32_PM_ENABLE`.
#[bitfield(u64, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct PmEnable {
    #[bit(0, rw)]
    hwp: bool,
}

/// `IA32_HWP_CAPABILITIES`: the performance levels this core allows.
#[bitfield(u64, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct HwpCapabilities {
    #[bits(0..=7, r)]
    highest: u8,
    #[bits(8..=15, r)]
    guaranteed: u8,
    #[bits(16..=23, r)]
    most_efficient: u8,
    #[bits(24..=31, r)]
    lowest: u8,
}

/// `IA32_HWP_REQUEST`: the range of levels the hardware picks from, and how it leans within it.
#[bitfield(u64, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct HwpRequest {
    #[bits(0..=7, rw)]
    minimum: u8,
    #[bits(8..=15, rw)]
    maximum: u8,
    /// 0 lets the hardware choose within the range.
    #[bits(16..=23, rw)]
    desired: u8,
    /// Energy-performance preference: 0 performance to 255 energy saving.
    #[bits(24..=31, rw)]
    energy_preference: u8,
    #[bits(32..=41, rw)]
    activity_window: u10,
    /// Follow `IA32_HWP_REQUEST_PKG` instead of this register.
    #[bit(42, rw)]
    package_control: bool,
}

/// `IA32_HWP_STATUS`.
#[bitfield(u64, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct HwpStatus {
    /// The guaranteed level changed.
    #[bit(0, r)]
    guaranteed_change: bool,
    /// The hardware went below the requested minimum.
    #[bit(2, r)]
    excursion_to_minimum: bool,
}

register!(
    PlatformInfo,
    PerfStatus,
    PerfCtl,
    MiscEnable,
    TemperatureTarget,
    ThermStatus,
    TurboRatioLimit,
    EnergyPerfBias,
    PmEnable,
    HwpCapabilities,
    HwpRequest,
    HwpStatus,
);

/// The frequency of a ratio of the bus clock.
#[must_use]
pub const fn ratio_mhz(ratio: u8) -> Mhz {
    Mhz(ratio as u32 * BUS_MHZ)
}
