//! RAPL: the energy counters and power limits of Intel and AMD processors.
//!
//! A domain's energy counter is 32 bits of a unit the power-unit register gives, and wraps; a
//! [`Counter`] keeps the running total across wraps so long as it is read more often than one wrap
//! takes (minutes at desktop power). Intel servers of the Haswell generation count DRAM in a fixed
//! 15.3 uJ unit instead, and Silvermont-generation Atoms scale every unit up rather than down,
//! which [`Flavor`] carries. AMD has package and per-core counters, no limits.

use arbitrary_int::{u4, u5, u6, u7, u15};
use bitbybit::bitfield;
use core::time::Duration;

use crate::msr::{Msr, register};
use crate::units::{MicroJoules, MilliWatts};
use ferrite_hal::flag;

pub const INTEL_POWER_UNIT: Msr<PowerUnit> = Msr::at(0x606);
pub const PKG_POWER_LIMIT: Msr<PackagePowerLimit> = Msr::at(0x610);
pub const PKG_ENERGY_STATUS: Msr<u64> = Msr::at(0x611);
/// Time the package spent throttled by RAPL, in time units; 32 bits.
pub const PKG_PERF_STATUS: Msr<u64> = Msr::at(0x613);
pub const PKG_POWER_INFO: Msr<PowerInfo> = Msr::at(0x614);
pub const DRAM_ENERGY_STATUS: Msr<u64> = Msr::at(0x619);
pub const PP0_ENERGY_STATUS: Msr<u64> = Msr::at(0x639);
pub const PP1_ENERGY_STATUS: Msr<u64> = Msr::at(0x641);
pub const PLATFORM_ENERGY_STATUS: Msr<u64> = Msr::at(0x64d);
pub const AMD_POWER_UNIT: Msr<PowerUnit> = Msr::at(0xc001_0299);
pub const AMD_CORE_ENERGY_STATUS: Msr<u64> = Msr::at(0xc001_029a);
pub const AMD_PKG_ENERGY_STATUS: Msr<u64> = Msr::at(0xc001_029b);

/// How a model's RAPL units scale.
#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::IntoStaticStr)]
#[strum(serialize_all = "kebab-case")]
pub enum Flavor {
    /// Units are 1/2^n of a joule, watt and second. Every AMD part with RAPL, too.
    Core,
    /// As [`Flavor::Core`], with DRAM energy in a fixed 15.3 uJ unit.
    HaswellServer,
    /// As [`Flavor::Core`], with platform energy in whole joules.
    SapphireRapidsServer,
    /// Energy in 2^n uJ, power in 2^n mW.
    Silvermont,
}

/// `MSR_RAPL_POWER_UNIT` and AMD's `RAPL_PWR_UNIT`, which share the layout.
#[bitfield(u64, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct PowerUnit {
    #[bits(0..=3, r)]
    power: u4,
    #[bits(8..=12, r)]
    energy: u5,
    #[bits(16..=19, r)]
    time: u4,
}

/// One energy domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::IntoStaticStr, strum::VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum Domain {
    /// The whole package.
    Package,
    /// The cores (Intel PP0; on AMD, the core this counter is read on).
    Cores,
    /// Integrated graphics (Intel PP1).
    Graphics,
    Dram,
    /// The whole platform as the board's sensor sees it (Intel `PSys`).
    Platform,
}

flag!(Domain);

impl Domain {
    /// The domain's counter on a vendor; `None` where the vendor has no such domain.
    #[must_use]
    pub const fn counter(self, amd: bool) -> Option<Msr<u64>> {
        match (self, amd) {
            (Self::Package, false) => Some(PKG_ENERGY_STATUS),
            (Self::Cores, false) => Some(PP0_ENERGY_STATUS),
            (Self::Graphics, false) => Some(PP1_ENERGY_STATUS),
            (Self::Dram, false) => Some(DRAM_ENERGY_STATUS),
            (Self::Platform, false) => Some(PLATFORM_ENERGY_STATUS),
            (Self::Package, true) => Some(AMD_PKG_ENERGY_STATUS),
            (Self::Cores, true) => Some(AMD_CORE_ENERGY_STATUS),
            _ => None,
        }
    }
}

/// The size of one count of an energy counter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnergyUnit {
    /// 1/2^n J.
    InverseShift(u8),
    /// 2^n uJ.
    MicroShift(u8),
    /// A fixed number of nanojoules.
    NanoJoules(u32),
}

impl EnergyUnit {
    #[must_use]
    pub fn to_micro(self, counts: u64) -> MicroJoules {
        let counts = u128::from(counts);
        let micro = match self {
            Self::InverseShift(n) => (counts * 1_000_000) >> n,
            Self::MicroShift(n) => counts << n,
            Self::NanoJoules(nj) => counts * u128::from(nj) / 1000,
        };
        MicroJoules(u64::try_from(micro).unwrap_or(u64::MAX))
    }
}

/// The units a [`PowerUnit`] value gives, as a flavor reads them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Units {
    pub flavor: Flavor,
    pub power_shift: u8,
    pub energy_shift: u8,
    pub time_shift: u8,
}

impl Units {
    #[must_use]
    pub fn new(flavor: Flavor, raw: PowerUnit) -> Self {
        Self {
            flavor,
            power_shift: raw.power().value(),
            energy_shift: raw.energy().value(),
            time_shift: raw.time().value(),
        }
    }

    #[must_use]
    pub const fn energy(self, domain: Domain) -> EnergyUnit {
        match (self.flavor, domain) {
            (Flavor::HaswellServer, Domain::Dram) => EnergyUnit::NanoJoules(15_300),
            (Flavor::SapphireRapidsServer, Domain::Platform) => EnergyUnit::NanoJoules(1_000_000_000),
            (Flavor::Silvermont, _) => EnergyUnit::MicroShift(self.energy_shift),
            _ => EnergyUnit::InverseShift(self.energy_shift),
        }
    }

    /// A power field's value.
    #[must_use]
    pub fn power(self, field: u32) -> MilliWatts {
        let mw = match self.flavor {
            Flavor::Silvermont => u64::from(field) << self.power_shift,
            _ => (u64::from(field) * 1000) >> self.power_shift,
        };
        MilliWatts(u32::try_from(mw).unwrap_or(u32::MAX))
    }

    /// A count of time units, 1/2^n s.
    #[must_use]
    pub fn time(self, counts: u64) -> Duration {
        Duration::from_micros(u64::try_from((u128::from(counts) * 1_000_000) >> self.time_shift).unwrap_or(u64::MAX))
    }

    /// A power limit's time window: 2^Y * (1 + Z/4) time units, Y in bits 4:0 and Z in bits 6:5.
    #[must_use]
    pub fn window(self, field: u7) -> Duration {
        let raw = field.value();
        let (y, z) = (u32::from(raw & 0x1f), u64::from(raw >> 5 & 0x3));
        let quarters = 1_u64.checked_shl(y).map_or(u64::MAX, |p| p.saturating_mul(4 + z));
        self.time(quarters / 4)
    }
}

/// `MSR_PKG_POWER_LIMIT`: the long-term (PL1) and short-term (PL2) package power limits.
#[bitfield(u64, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct PackagePowerLimit {
    #[bits(0..=14, r)]
    pl1_power: u15,
    #[bit(15, r)]
    pl1_enabled: bool,
    #[bit(16, r)]
    pl1_clamp: bool,
    #[bits(17..=23, r)]
    pl1_window: u7,
    #[bits(32..=46, r)]
    pl2_power: u15,
    #[bit(47, r)]
    pl2_enabled: bool,
    #[bit(48, r)]
    pl2_clamp: bool,
    #[bits(49..=55, r)]
    pl2_window: u7,
    /// The firmware locked the limits until reset.
    #[bit(63, r)]
    locked: bool,
}

/// `MSR_PKG_POWER_INFO`.
#[bitfield(u64, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct PowerInfo {
    /// The thermal design power.
    #[bits(0..=14, r)]
    thermal_spec: u15,
    #[bits(16..=30, r)]
    minimum: u15,
    #[bits(32..=46, r)]
    maximum: u15,
    #[bits(48..=53, r)]
    max_window: u6,
}

register!(PowerUnit, PackagePowerLimit, PowerInfo);

/// One power limit, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limit {
    pub power: MilliWatts,
    pub window: Duration,
    pub enabled: bool,
}

/// The package's power limits and design power, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackageLimits {
    pub pl1: Limit,
    pub pl2: Limit,
    pub locked: bool,
    pub thermal_design: Option<MilliWatts>,
}

impl PackageLimits {
    #[must_use]
    pub fn decode(units: Units, limit: PackagePowerLimit, info: Option<PowerInfo>) -> Self {
        let tdp = info.map(|i| units.power(u32::from(i.thermal_spec().value()))).filter(|p| p.0 != 0);
        Self {
            pl1: Limit {
                power: units.power(u32::from(limit.pl1_power().value())),
                window: units.window(limit.pl1_window()),
                enabled: limit.pl1_enabled(),
            },
            pl2: Limit {
                power: units.power(u32::from(limit.pl2_power().value())),
                window: units.window(limit.pl2_window()),
                enabled: limit.pl2_enabled(),
            },
            locked: limit.locked(),
            thermal_design: tdp,
        }
    }
}

/// A 32-bit counter's running total across wraps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counter {
    last: Option<u32>,
    total: u64,
}

impl Counter {
    /// Take a new raw reading; returns the counts since the previous one (0 for the first).
    pub fn update(&mut self, raw: u64) -> u64 {
        let now = raw as u32;
        let delta = self.last.map_or(0, |last| u64::from(now.wrapping_sub(last)));
        self.last = Some(now);
        self.total += delta;
        delta
    }

    #[must_use]
    pub const fn total(self) -> u64 {
        self.total
    }
}
