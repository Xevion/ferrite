//! Asking for a clock range, in terms that hold across Intel and AMD.
//!
//! A [`Request`] names a floor, a ceiling and an energy preference as [`Bound`]s (named levels or a
//! clock); [`Interface::apply`] resolves them against the calling core's own levels, writes the
//! registers, reads them back and keeps the values it replaced so [`Saved::restore`] can put them
//! back. It runs on each core, since levels differ per core on hybrid and preferred-core parts.

use core::fmt;

use arbitrary_int::u4;
use snafu::Snafu;

use crate::amd::{self, CppcCapabilities, CppcEnable, CppcRequest, Hwcr, PStateCtl};
use crate::cpuid::{CoreType, CpuidSource, Feature, Features};
use crate::intel::{self, EnergyPerfBias, HwpCapabilities, PerfCtl, PmEnable};
use crate::msr::{Fault, MsrIo};
use crate::units::Mhz;

/// The kHz of one ratio of the 100 MHz bus, the HWP unit on all but hybrid P-cores.
const BUS_KHZ: u32 = intel::BUS_MHZ * 1000;

/// An energy-performance preference: 0 favours performance, 255 energy saving.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Epp(pub u8);

impl Epp {
    pub const PERFORMANCE: Self = Self(0);
    pub const BALANCE_PERFORMANCE: Self = Self(0x80);
    pub const BALANCE_POWER: Self = Self(0xc0);
    pub const POWER: Self = Self(0xff);

    /// The same lean on `IA32_ENERGY_PERF_BIAS`'s 0 to 15 scale.
    #[must_use]
    pub fn as_bias(self) -> u4 {
        u4::new(((u16::from(self.0) * 15 + 127) / 255) as u8)
    }
}

/// A performance level a request names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bound {
    Lowest,
    /// The lowest level that still saves energy per unit of work.
    Efficient,
    /// The base clock.
    Nominal,
    /// The top of what the core allows, boost included.
    Highest,
    /// A level in the interface's own units.
    Level(u8),
    /// A clock, rounded down to a level; needs the level's scale.
    Clock(Mhz),
}

impl fmt::Display for Bound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lowest => f.write_str("lowest"),
            Self::Efficient => f.write_str("efficient"),
            Self::Nominal => f.write_str("nominal"),
            Self::Highest => f.write_str("highest"),
            Self::Level(level) => write!(f, "level {level}"),
            Self::Clock(mhz) => write!(f, "{mhz}"),
        }
    }
}

/// What to ask every core for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    pub floor: Bound,
    pub ceiling: Bound,
    pub preference: Epp,
}

impl Request {
    /// The highest level only, leaning to performance: what an OS performance governor asks for.
    pub const MAX: Self = Self { floor: Bound::Highest, ceiling: Bound::Highest, preference: Epp::PERFORMANCE };

    /// Anywhere from the lowest level up to `ceiling`, leaning to performance within it.
    #[must_use]
    pub const fn capped(ceiling: Bound) -> Self {
        Self { floor: Bound::Lowest, ceiling, preference: Epp::PERFORMANCE }
    }
}

impl fmt::Display for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} to {}, preference {}", self.floor, self.ceiling, self.preference.0)
    }
}

/// One core's performance levels in its interface's units, higher faster, and what one unit is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Levels {
    pub lowest: u8,
    pub efficient: u8,
    pub nominal: u8,
    pub highest: u8,
    /// One level's frequency; `None` where the part does not say.
    pub khz_per_level: Option<u32>,
}

impl Levels {
    #[must_use]
    pub fn hwp(caps: HwpCapabilities, khz_per_level: Option<u32>) -> Self {
        Self {
            lowest: caps.lowest(),
            efficient: caps.most_efficient(),
            nominal: caps.guaranteed(),
            highest: caps.highest(),
            khz_per_level,
        }
    }

    /// CPPC levels; `nominal` (P0's clock) gives the scale.
    #[must_use]
    pub fn cppc(caps: CppcCapabilities, nominal: Option<Mhz>) -> Self {
        let per = nominal.filter(|_| caps.nominal() != 0).map(|m| m.0 * 1000 / u32::from(caps.nominal()));
        Self {
            lowest: caps.lowest(),
            efficient: caps.lowest_nonlinear(),
            nominal: caps.nominal(),
            highest: caps.highest(),
            khz_per_level: per,
        }
    }

    /// Bus ratios.
    #[must_use]
    pub const fn ratios(efficient: u8, base: u8, turbo: u8) -> Self {
        Self { lowest: efficient, efficient, nominal: base, highest: turbo, khz_per_level: Some(BUS_KHZ) }
    }

    #[must_use]
    pub fn mhz(&self, level: u8) -> Option<Mhz> {
        self.khz_per_level.map(|k| Mhz(u32::from(level) * k / 1000))
    }

    /// The level `bound` names, inside lowest to highest.
    pub fn resolve(&self, bound: Bound) -> Result<u8, ApplyError> {
        let level = match bound {
            Bound::Lowest => self.lowest,
            Bound::Efficient => self.efficient,
            Bound::Nominal => self.nominal,
            Bound::Highest => self.highest,
            Bound::Level(level) => level,
            Bound::Clock(mhz) => {
                let per = self.khz_per_level.filter(|&k| k != 0).ok_or(ApplyError::NoScale { bound })?;
                u8::try_from(u64::from(mhz.0) * 1000 / u64::from(per)).unwrap_or(u8::MAX)
            },
        };
        Ok(level.clamp(self.lowest.min(self.highest), self.highest))
    }
}

impl fmt::Display for Levels {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let named = [
            ("lowest", self.lowest),
            ("efficient", self.efficient),
            ("nominal", self.nominal),
            ("highest", self.highest),
        ];
        for (i, (name, level)) in named.into_iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            match self.mhz(level) {
                Some(mhz) => write!(f, "{name} {level} ({mhz})")?,
                None => write!(f, "{name} {level}")?,
            }
        }
        Ok(())
    }
}

/// AMD's hardware P-states: index 0 fastest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PStates {
    /// Each enabled state's clock, by index.
    pub clocks: [Option<Mhz>; amd::PSTATES as usize],
    /// Boost runs above P0 when `HWCR` allows it.
    pub boost: bool,
}

impl PStates {
    /// The fastest state no faster than `bound`, and whether boost may run.
    fn choose(&self, bound: Bound) -> Result<(u8, bool), ApplyError> {
        let enabled = || self.clocks.iter().enumerate().filter_map(|(i, c)| c.map(|c| (i as u8, c)));
        let slowest = enabled().next_back().map_or(0, |(i, _)| i);
        Ok(match bound {
            Bound::Highest => (0, self.boost),
            Bound::Nominal => (0, false),
            Bound::Lowest | Bound::Efficient => (slowest, false),
            Bound::Level(index) => (index.min(slowest), false),
            Bound::Clock(mhz) => {
                let p0 = self.clocks[0].ok_or(ApplyError::NoScale { bound })?;
                if mhz > p0 {
                    (0, self.boost)
                } else {
                    (enabled().find(|&(_, c)| c <= mhz).map_or(slowest, |(i, _)| i), false)
                }
            },
        })
    }
}

/// How the processor's clock is asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interface {
    /// Intel hardware P-states.
    Hwp {
        /// One P-core level's kHz on a hybrid part, from the model table; `None` when unknown.
        p_core_khz: Option<u32>,
        features: Features,
    },
    /// Intel `IA32_PERF_CTL` ratios.
    PerfCtl { levels: Levels, features: Features },
    /// AMD CPPC through MSRs; `nominal` is P0's clock where the P-state registers give it.
    Cppc { nominal: Option<Mhz> },
    /// AMD `PStateCtl`.
    AmdPState(PStates),
}

impl fmt::Display for Interface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hwp { features, .. } => {
                write!(f, "Intel HWP{}", if features.has(Feature::HwpEpp) { " with EPP" } else { "" })
            },
            Self::PerfCtl { levels, .. } => write!(f, "Intel PERF_CTL ratios ({levels})"),
            Self::Cppc { nominal: Some(mhz) } => write!(f, "AMD CPPC, nominal {mhz}"),
            Self::Cppc { nominal: None } => f.write_str("AMD CPPC, nominal clock unknown"),
            Self::AmdPState(states) => {
                f.write_str("AMD P-states")?;
                for (i, mhz) in states.clocks.iter().enumerate().filter_map(|(i, c)| c.map(|c| (i, c))) {
                    write!(f, " P{i} {mhz}")?;
                }
                if states.boost { f.write_str(", boost") } else { Ok(()) }
            },
        }
    }
}

#[derive(Debug, Snafu, Clone, Copy, PartialEq, Eq)]
pub enum ApplyError {
    #[snafu(display("{source}"))]
    Msr { source: Fault },
    #[snafu(display("{bound} needs a level's clock, which this processor does not give"))]
    NoScale { bound: Bound },
}

impl From<Fault> for ApplyError {
    fn from(source: Fault) -> Self {
        Self::Msr { source }
    }
}

/// Registers a request replaced, oldest first, to write back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Saved {
    writes: [Option<(u32, u64)>; 3],
}

impl Saved {
    fn keep(&mut self, address: u32, old: u64) {
        if let Some(slot) = self.writes.iter_mut().find(|w| w.is_none()) {
            *slot = Some((address, old));
        }
    }

    /// Write every replaced register back, newest first. The enables (HWP, CPPC) stay on, since
    /// nothing but a reset turns them off.
    pub fn restore(&self, msr: &mut impl MsrIo) -> Result<(), Fault> {
        self.writes.iter().rev().flatten().try_for_each(|&(address, old)| msr.write_raw(address, old))
    }
}

/// What a core was asked for and what it took.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Applied {
    /// The core's levels; `None` for P-states.
    pub levels: Option<Levels>,
    pub written: u64,
    pub readback: u64,
    pub saved: Saved,
}

impl Applied {
    /// Whether the request register reads back as written, in its low 32 bits.
    #[must_use]
    pub const fn took(&self) -> bool {
        self.written as u32 == self.readback as u32
    }
}

impl Interface {
    /// Ask the calling core for `request`. Every register was found on the boot processor by
    /// [`crate::discover::Platform::discover`], which holds for every core of the package.
    pub fn apply(&self, request: Request, cpu: &impl CpuidSource, msr: &mut impl MsrIo) -> Result<Applied, ApplyError> {
        let mut saved = Saved::default();
        match *self {
            Self::Hwp { p_core_khz, features } => {
                if !msr.read(intel::IA32_PM_ENABLE)?.hwp() {
                    msr.write(intel::IA32_PM_ENABLE, PmEnable::DEFAULT.with_hwp(true))?;
                }
                let khz = match (features.has(Feature::Hybrid), CoreType::read(cpu, features)) {
                    (false, _) | (true, Some(CoreType::Efficient)) => Some(BUS_KHZ),
                    (true, Some(CoreType::Performance)) => p_core_khz,
                    (true, None) => None,
                };
                let levels = Levels::hwp(msr.read(intel::IA32_HWP_CAPABILITIES)?, khz);
                let old = msr.read(intel::IA32_HWP_REQUEST)?;
                let mut new = old
                    .with_minimum(levels.resolve(request.floor)?)
                    .with_maximum(levels.resolve(request.ceiling)?)
                    .with_desired(0)
                    .with_package_control(false);
                if features.has(Feature::HwpEpp) {
                    new = new.with_energy_preference(request.preference.0);
                } else if features.has(Feature::Epb) {
                    write_bias(msr, request.preference, &mut saved)?;
                }
                let written =
                    finish(msr, intel::IA32_HWP_REQUEST.address(), old.raw_value(), new.raw_value(), &mut saved)?;
                Ok(Applied {
                    levels: Some(levels),
                    written,
                    readback: msr.read_raw(intel::IA32_HWP_REQUEST.address())?,
                    saved,
                })
            },
            Self::PerfCtl { levels, features } => {
                if features.has(Feature::Epb) {
                    write_bias(msr, request.preference, &mut saved)?;
                }
                let old: PerfCtl = msr.read(intel::IA32_PERF_CTL)?;
                let new = old.with_target_ratio(levels.resolve(request.ceiling)?);
                let written =
                    finish(msr, intel::IA32_PERF_CTL.address(), old.raw_value(), new.raw_value(), &mut saved)?;
                Ok(Applied {
                    levels: Some(levels),
                    written,
                    readback: msr.read_raw(intel::IA32_PERF_CTL.address())?,
                    saved,
                })
            },
            Self::Cppc { nominal } => {
                let levels = Levels::cppc(msr.read(amd::CPPC_CAP1)?, nominal);
                if !msr.read(amd::CPPC_ENABLE)?.enable() {
                    msr.write(amd::CPPC_ENABLE, CppcEnable::DEFAULT.with_enable(true))?;
                }
                let old: CppcRequest = msr.read(amd::CPPC_REQ)?;
                let new = old
                    .with_minimum(levels.resolve(request.floor)?)
                    .with_maximum(levels.resolve(request.ceiling)?)
                    .with_desired(0)
                    .with_energy_preference(request.preference.0);
                let written = finish(msr, amd::CPPC_REQ.address(), old.raw_value(), new.raw_value(), &mut saved)?;
                Ok(Applied { levels: Some(levels), written, readback: msr.read_raw(amd::CPPC_REQ.address())?, saved })
            },
            Self::AmdPState(states) => {
                let (index, boost) = states.choose(request.ceiling)?;
                let limit = msr.read(amd::PSTATE_LIMIT)?.current_limit().value();
                let index = index.max(limit);
                if states.boost {
                    let old: Hwcr = msr.read(amd::HWCR)?;
                    let new = old.with_cpb_disable(!boost);
                    if new != old {
                        msr.write(amd::HWCR, new)?;
                        saved.keep(amd::HWCR.address(), old.raw_value());
                    }
                }
                let old: PStateCtl = msr.read(amd::PSTATE_CTL)?;
                let new = old.with_command(arbitrary_int::u3::new(index));
                let written = finish(msr, amd::PSTATE_CTL.address(), old.raw_value(), new.raw_value(), &mut saved)?;
                Ok(Applied { levels: None, written, readback: msr.read_raw(amd::PSTATE_CTL.address())?, saved })
            },
        }
    }
}

/// Write `new` over `old` at `address` when they differ, keeping `old`; returns what is in force.
fn finish(msr: &mut impl MsrIo, address: u32, old: u64, new: u64, saved: &mut Saved) -> Result<u64, Fault> {
    if new != old {
        msr.write_raw(address, new)?;
        saved.keep(address, old);
    }
    Ok(new)
}

fn write_bias(msr: &mut impl MsrIo, preference: Epp, saved: &mut Saved) -> Result<(), Fault> {
    let old: EnergyPerfBias = msr.read(intel::IA32_ENERGY_PERF_BIAS)?;
    let new = old.with_bias(preference.as_bias());
    finish(msr, intel::IA32_ENERGY_PERF_BIAS.address(), old.raw_value(), new.raw_value(), saved).map(drop)
}
