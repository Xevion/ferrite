//! Why the processor holds its clock below what software asked for.
//!
//! Intel reports it per package in a limit-reasons register: each status bit says a limit is
//! clipping the clock now, and the log bit 16 above it stays set until software writes 0. Haswell
//! and Broadwell use `MSR_CORE_PERF_LIMIT_REASONS` at 0x690; Skylake and later client parts use
//! 0x64F with a different set of reasons. AMD has no such register.

use bitbybit::bitfield;

use crate::msr::{Msr, register};
use crate::set::{Set, flag};

/// The Haswell-generation register.
pub const HASWELL_LIMIT_REASONS: Msr<HaswellReasons> = Msr::at(0x690);
/// The Skylake-generation register.
pub const SKYLAKE_LIMIT_REASONS: Msr<SkylakeReasons> = Msr::at(0x64f);

/// Which register a model has.
#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::IntoStaticStr)]
#[strum(serialize_all = "kebab-case")]
pub enum Layout {
    Haswell,
    Skylake,
}

impl Layout {
    #[must_use]
    pub const fn address(self) -> u32 {
        match self {
            Self::Haswell => HASWELL_LIMIT_REASONS.address(),
            Self::Skylake => SKYLAKE_LIMIT_REASONS.address(),
        }
    }

    /// The reasons in a raw register value read from this layout's register.
    #[must_use]
    pub fn decode(self, raw: u64) -> Reasons {
        match self {
            Self::Haswell => HaswellReasons::new_with_raw_value(raw).reasons(),
            Self::Skylake => SkylakeReasons::new_with_raw_value(raw).reasons(),
        }
    }
}

/// One reason for a clipped clock, across both layouts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::IntoStaticStr, strum::VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum Limit {
    /// An external PROCHOT assertion.
    Prochot,
    /// The die reached its thermal limit.
    Thermal,
    /// Graphics took the package's power (Haswell layout).
    Graphics,
    /// HWP's own choice (Haswell layout).
    AutoHwp,
    /// Package C-state residency regulation.
    ResidencyRegulation,
    /// The running-average thermal limit.
    RunningAverageThermal,
    /// A voltage regulator's thermal alert.
    VrThermal,
    /// A voltage regulator's design current limit.
    VrCurrent,
    /// Current or another electrical constraint.
    Electrical,
    /// The cores' own power limit (Haswell layout).
    CorePower,
    /// The long-term package power limit, PL1.
    PowerPl1,
    /// The short-term package power limits, PL2 and PL3.
    PowerPl2,
    /// The turbo ratio allowed for this many active cores.
    MultiCoreTurbo,
    /// Turbo transitions being damped.
    TurboAttenuation,
}

flag!(Limit);

pub type Limits = Set<Limit>;

/// What a limit-reasons register says: the limits clipping the clock now, and every limit that has
/// since the log was last cleared.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reasons {
    pub active: Limits,
    pub logged: Limits,
}

/// `MSR_CORE_PERF_LIMIT_REASONS` on Haswell and Broadwell, as `turbostat` decodes it.
#[bitfield(u64, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct HaswellReasons {
    #[bit(0, r)]
    prochot: bool,
    #[bit(1, r)]
    thermal: bool,
    #[bit(4, r)]
    graphics: bool,
    #[bit(5, r)]
    auto_hwp: bool,
    #[bit(6, r)]
    vr_thermal: bool,
    #[bit(8, r)]
    electrical: bool,
    #[bit(9, r)]
    core_power: bool,
    #[bit(10, r)]
    pl1: bool,
    #[bit(11, r)]
    pl2: bool,
    #[bit(12, r)]
    multi_core_turbo: bool,
    #[bit(13, r)]
    turbo_attenuation: bool,
}

impl HaswellReasons {
    fn limits(self) -> Limits {
        [
            (Limit::Prochot, self.prochot()),
            (Limit::Thermal, self.thermal()),
            (Limit::Graphics, self.graphics()),
            (Limit::AutoHwp, self.auto_hwp()),
            (Limit::VrThermal, self.vr_thermal()),
            (Limit::Electrical, self.electrical()),
            (Limit::CorePower, self.core_power()),
            (Limit::PowerPl1, self.pl1()),
            (Limit::PowerPl2, self.pl2()),
            (Limit::MultiCoreTurbo, self.multi_core_turbo()),
            (Limit::TurboAttenuation, self.turbo_attenuation()),
        ]
        .into_iter()
        .filter_map(|(limit, set)| set.then_some(limit))
        .collect()
    }

    #[must_use]
    pub fn reasons(self) -> Reasons {
        Reasons { active: self.limits(), logged: Self::new_with_raw_value(self.raw_value() >> 16).limits() }
    }
}

/// `MSR_CORE_PERF_LIMIT_REASONS` on Skylake and later client parts, as EDK2's Skylake MSR header
/// lays it out.
#[bitfield(u64, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct SkylakeReasons {
    #[bit(0, r)]
    prochot: bool,
    #[bit(1, r)]
    thermal: bool,
    #[bit(4, r)]
    residency_regulation: bool,
    #[bit(5, r)]
    running_average_thermal: bool,
    #[bit(6, r)]
    vr_thermal: bool,
    #[bit(7, r)]
    vr_current: bool,
    #[bit(8, r)]
    electrical: bool,
    #[bit(10, r)]
    pl1: bool,
    #[bit(11, r)]
    pl2: bool,
    #[bit(12, r)]
    max_turbo: bool,
    #[bit(13, r)]
    turbo_attenuation: bool,
}

impl SkylakeReasons {
    fn limits(self) -> Limits {
        [
            (Limit::Prochot, self.prochot()),
            (Limit::Thermal, self.thermal()),
            (Limit::ResidencyRegulation, self.residency_regulation()),
            (Limit::RunningAverageThermal, self.running_average_thermal()),
            (Limit::VrThermal, self.vr_thermal()),
            (Limit::VrCurrent, self.vr_current()),
            (Limit::Electrical, self.electrical()),
            (Limit::PowerPl1, self.pl1()),
            (Limit::PowerPl2, self.pl2()),
            (Limit::MultiCoreTurbo, self.max_turbo()),
            (Limit::TurboAttenuation, self.turbo_attenuation()),
        ]
        .into_iter()
        .filter_map(|(limit, set)| set.then_some(limit))
        .collect()
    }

    #[must_use]
    pub fn reasons(self) -> Reasons {
        Reasons { active: self.limits(), logged: Self::new_with_raw_value(self.raw_value() >> 16).limits() }
    }
}

register!(HaswellReasons, SkylakeReasons);
