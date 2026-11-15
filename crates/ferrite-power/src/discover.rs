//! What this processor's power registers are, found without writing any of them.

use core::fmt;

use snafu::Snafu;

use crate::amd::{self, PStateDef};
use crate::control::{Interface, Levels, PStates};
use crate::cpuid::{Cpu, CpuidSource, Feature, Vendor};
use crate::intel::models::Model;
use crate::intel::{self, MiscEnable, PlatformInfo, TurboRatioLimit};
use crate::limits::Layout;
use crate::msr::{Fault, MsrIo};
use crate::rapl::{self, Domain, Flavor, PackageLimits, Units};
use crate::units::Mhz;
use ferrite_hal::Set;

/// Why the clock cannot be asked for here.
#[derive(Debug, Snafu, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    #[snafu(display("running under a hypervisor"))]
    Hypervisor,
    #[snafu(display("not an Intel or AMD processor"))]
    Vendor,
    #[snafu(display("Intel family {family} model {model:#x} is not in the model table"))]
    UnknownModel { family: u16, model: u8 },
    #[snafu(display("no HWP, and no Enhanced SpeedStep with known ratio registers"))]
    NoIntelInterface,
    #[snafu(display("no CPPC MSRs and no hardware P-states (family {family:#x})"))]
    NoAmdInterface { family: u16 },
    #[snafu(display("reading the clock registers: {source}"))]
    Read { source: Fault },
}

/// The energy counters there are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Energy {
    pub units: Units,
    pub domains: Set<Domain>,
    pub amd: bool,
}

/// What the firmware left the clock at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Firmware {
    /// HWP or CPPC was already on.
    pub enabled: bool,
    /// Intel turbo turned off in `IA32_MISC_ENABLE`, or AMD boost in `HWCR`.
    pub turbo_off: bool,
    /// The boot processor's clock request as the firmware left it, `IA32_PERF_STATUS` on Intel.
    pub current: Option<Mhz>,
}

/// Everything found about the processor's clock, energy and limit registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Platform {
    pub cpu: Cpu,
    /// The Intel model, when it is in the table.
    pub model: Option<Model>,
    pub control: Result<Interface, Refusal>,
    pub firmware: Option<Firmware>,
    pub energy: Option<Energy>,
    pub package_limits: Option<PackageLimits>,
    pub limit_reasons: Option<Layout>,
    /// `MSR_PPERF` counts.
    pub pperf: bool,
    /// `IA32_APERF` and `IA32_MPERF` count.
    pub aperf_mperf: bool,
}

impl Platform {
    /// Find what the processor has, on the boot processor. Reads only; a register that faults is
    /// recorded as absent.
    pub fn discover(cpuid: &impl CpuidSource, msr: &mut impl MsrIo) -> Self {
        let cpu = Cpu::read(cpuid);
        let model = cpu.intel_family6_model().and_then(Model::of);
        let mut platform = Self {
            cpu,
            model,
            control: Err(Refusal::Hypervisor),
            firmware: None,
            energy: None,
            package_limits: None,
            limit_reasons: None,
            pperf: false,
            aperf_mperf: false,
        };
        if cpu.features.has(Feature::Hypervisor) {
            return platform;
        }
        platform.aperf_mperf = cpu.features.has(Feature::AperfMperf) && msr.read(intel::IA32_APERF).is_ok();
        match cpu.vendor {
            Vendor::Intel => platform.discover_intel(msr),
            Vendor::Amd => platform.discover_amd(msr),
            Vendor::Other(_) => platform.control = Err(Refusal::Vendor),
        }
        platform
    }

    fn discover_intel(&mut self, msr: &mut impl MsrIo) {
        let features = self.cpu.features;
        let misc: Option<MiscEnable> = msr.read(intel::IA32_MISC_ENABLE).ok();
        let current = msr.read(intel::IA32_PERF_STATUS).ok().map(|s| intel::ratio_mhz(s.current_ratio()));
        let turbo_off = misc.is_some_and(|m| m.turbo_disable());
        self.control = if features.has(Feature::Hwp) {
            match msr.read(intel::IA32_PM_ENABLE) {
                Ok(enable) => {
                    self.firmware = Some(Firmware { enabled: enable.hwp(), turbo_off, current });
                    Ok(Interface::Hwp { p_core_khz: self.model.and_then(Model::p_core_khz_per_unit), features })
                },
                Err(source) => Err(Refusal::Read { source }),
            }
        } else {
            match self.model {
                None => {
                    Err(Refusal::UnknownModel { family: self.cpu.signature.family, model: self.cpu.signature.model })
                },
                Some(model) if features.has(Feature::Eist) && model.has_core_ratios() => {
                    self.firmware = Some(Firmware { enabled: false, turbo_off, current });
                    perf_ctl_levels(msr, turbo_off)
                        .map(|levels| Interface::PerfCtl { levels, features })
                        .map_err(|source| Refusal::Read { source })
                },
                Some(_) => Err(Refusal::NoIntelInterface),
            }
        };
        let Some(model) = self.model else { return };
        self.energy = energy(msr, model.rapl(), false);
        if let Some(energy) = self.energy {
            let limit = msr.read(rapl::PKG_POWER_LIMIT).ok();
            let info = msr.read(rapl::PKG_POWER_INFO).ok();
            self.package_limits = limit.map(|l| PackageLimits::decode(energy.units, l, info));
        }
        self.limit_reasons = model.limit_reasons().filter(|l| msr.read_raw(l.address()).is_ok());
        self.pperf = model.may_have_pperf() && msr.read(intel::PPERF).is_ok_and(|v| v != 0);
    }

    fn discover_amd(&mut self, msr: &mut impl MsrIo) {
        let features = self.cpu.features;
        let family = self.cpu.signature.family;
        let states = features.has(Feature::AmdHwPstate).then(|| pstates(msr, family, features.has(Feature::AmdCpb)));
        let turbo_off = msr.read(amd::HWCR).is_ok_and(|h| h.cpb_disable());
        let current = states.and_then(|s| {
            let index = msr.read(amd::PSTATE_STATUS).ok()?.current().value();
            s.clocks.get(usize::from(index)).copied().flatten()
        });
        self.control = if features.has(Feature::AmdCppc) {
            match msr.read(amd::CPPC_ENABLE) {
                Ok(enable) => {
                    self.firmware = Some(Firmware { enabled: enable.enable(), turbo_off, current });
                    Ok(Interface::Cppc { nominal: states.and_then(|s| s.clocks[0]) })
                },
                Err(source) => Err(Refusal::Read { source }),
            }
        } else if let Some(states) = states.filter(|s| s.clocks.iter().any(Option::is_some)) {
            self.firmware = Some(Firmware { enabled: false, turbo_off, current });
            Ok(Interface::AmdPState(states))
        } else {
            Err(Refusal::NoAmdInterface { family })
        };
        if features.has(Feature::AmdRapl) {
            self.energy = energy(msr, Flavor::Core, true);
        }
    }
}

/// The ratios `IA32_PERF_CTL` takes: efficient floor, base, and single-core turbo unless the
/// firmware turned turbo off.
fn perf_ctl_levels(msr: &mut impl MsrIo, turbo_off: bool) -> Result<Levels, Fault> {
    let info: PlatformInfo = msr.read(intel::PLATFORM_INFO)?;
    let base = info.max_non_turbo_ratio();
    let turbo = if turbo_off {
        None
    } else {
        msr.read(intel::TURBO_RATIO_LIMIT).ok().and_then(|t: TurboRatioLimit| t.with_active(1))
    };
    let efficient = match info.max_efficiency_ratio() {
        0 => base,
        ratio => ratio,
    };
    Ok(Levels::ratios(efficient, base, turbo.unwrap_or(base).max(base)))
}

/// Every enabled P-state from P0 to the slowest `PStateCurLim` names.
fn pstates(msr: &mut impl MsrIo, family: u16, boost: bool) -> PStates {
    let slowest = msr.read(amd::PSTATE_LIMIT).map_or(amd::PSTATES - 1, |l| l.max_value().value());
    let mut clocks = [None; amd::PSTATES as usize];
    for (index, clock) in (0..=slowest.min(amd::PSTATES - 1)).zip(clocks.iter_mut()) {
        *clock = msr.read(amd::PSTATE_DEF.offset(u32::from(index))).ok().and_then(|d: PStateDef| d.frequency(family));
    }
    PStates { clocks, boost }
}

/// The energy counters that read, with the units to scale them; `None` without a package counter.
fn energy(msr: &mut impl MsrIo, flavor: Flavor, amd: bool) -> Option<Energy> {
    let unit = if amd { rapl::AMD_POWER_UNIT } else { rapl::INTEL_POWER_UNIT };
    let units = Units::new(flavor, msr.read(unit).ok()?);
    let domains: Set<Domain> = <Domain as strum::VariantArray>::VARIANTS
        .iter()
        .copied()
        // AMD's core counter is per core, so the boot processor's would read as the package's cores.
        .filter(|&d| !(amd && d == Domain::Cores))
        .filter(|&d| d.counter(amd).is_some_and(|m| msr.read(m).is_ok()))
        .collect();
    domains.has(Domain::Package).then_some(Energy { units, domains, amd })
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.cpu)?;
        if let Some(model) = self.model {
            write!(f, " ({})", <&str>::from(model))?;
        }
        match &self.control {
            Ok(interface) => write!(f, "; clock through {interface}")?,
            Err(why) => write!(f, "; clock left alone: {why}")?,
        }
        if let Some(fw) = self.firmware {
            write!(f, "; firmware: {}", if fw.enabled { "enabled" } else { "not enabled" })?;
            if fw.turbo_off {
                f.write_str(", turbo off")?;
            }
            if let Some(mhz) = fw.current {
                write!(f, ", at {mhz}")?;
            }
        }
        match self.energy {
            Some(e) => write!(f, "; energy: {} ({})", e.domains, <&str>::from(e.units.flavor))?,
            None => f.write_str("; no energy counters")?,
        }
        if let Some(l) = self.package_limits {
            write!(f, "; PL1 {} over {:?}, PL2 {}", l.pl1.power, l.pl1.window, l.pl2.power)?;
            if l.locked {
                f.write_str(" (locked)")?;
            }
        }
        match self.limit_reasons {
            Some(layout) => write!(f, "; limit reasons ({})", <&str>::from(layout))?,
            None => f.write_str("; no limit reasons")?,
        }
        if self.pperf {
            f.write_str("; PPERF")?;
        }
        Ok(())
    }
}
