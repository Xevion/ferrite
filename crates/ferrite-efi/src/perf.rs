//! The processor's clock request, raised to the highest level the processor allows.
//!
//! Firmware leaves every core at whatever performance state it booted in, which on a laptop can be below the base clock. [`Plan::probe`] reads what the processor allows
//! and [`Plan::raise`] asks every core for the most of it, as an operating system's performance
//! governor would. The processor still lowers its clock on its own for heat and power.
//!
//! With HWP (CPUID 6 EAX bit 7) the request is `IA32_HWP_REQUEST` with its minimum and maximum at
//! the highest performance level and the energy preference at performance; once enabled through
//! `IA32_PM_ENABLE`, HWP stays on until the next reset. Without it the request is the ratio in
//! `IA32_PERF_CTL`: the single-core turbo ratio when turbo is enabled, else the base ratio.
//!
//! Intel family 6 only, and never under a hypervisor: every MSR here is gated on the CPUID bits and
//! models that say it exists, since nothing can recover from the fault an absent MSR raises.

use core::arch::x86_64::__cpuid;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use core::time::Duration;

use crate::mp::{ApTask, Mp, ProcessorNumber, StackUse};
use crate::paging::{read_msr, write_msr};

const MSR_PLATFORM_INFO: u32 = 0xce;
const IA32_PERF_STATUS: u32 = 0x198;
const IA32_PERF_CTL: u32 = 0x199;
const IA32_MISC_ENABLE: u32 = 0x1a0;
const MSR_TURBO_RATIO_LIMIT: u32 = 0x1ad;
const IA32_PM_ENABLE: u32 = 0x770;
const IA32_HWP_CAPABILITIES: u32 = 0x771;
const IA32_HWP_REQUEST: u32 = 0x774;

/// `IA32_MISC_ENABLE` bit 38: the firmware has turned turbo off.
const TURBO_DISABLE: u64 = 1 << 38;
/// The bus clock that ratios multiply on every Intel core since Sandy Bridge.
const BUS_MHZ: u32 = 100;
/// How long an AP may take to write two MSRs.
const AP_TIMEOUT: Duration = Duration::from_secs(1);

/// Atom-line models of family 6 from Silvermont on, whose ratio MSRs differ from the Core line's.
const ATOMS: [u32; 12] = [0x37, 0x4a, 0x4c, 0x4d, 0x5a, 0x5c, 0x5f, 0x7a, 0x86, 0x96, 0x9c, 0xbe];
/// Sandy Bridge, the first model with `MSR_PLATFORM_INFO` and `MSR_TURBO_RATIO_LIMIT` as used here.
const FIRST_MODEL: u32 = 0x2a;

/// A performance level: a multiple of the 100 MHz bus clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Ratio(u8);

impl Ratio {
    #[must_use]
    pub const fn new(raw: u8) -> Option<Self> {
        if raw == 0 { None } else { Some(Self(raw)) }
    }

    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }

    #[must_use]
    pub const fn mhz(self) -> u32 {
        self.0 as u32 * BUS_MHZ
    }
}

impl fmt::Display for Ratio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} MHz", self.mhz())
    }
}

/// The performance levels `IA32_HWP_CAPABILITIES` reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HwpLevels {
    pub highest: u8,
    pub guaranteed: u8,
    pub efficient: u8,
    pub lowest: u8,
}

impl HwpLevels {
    #[must_use]
    pub const fn decode(raw: u64) -> Self {
        Self {
            highest: raw as u8,
            guaranteed: (raw >> 8) as u8,
            efficient: (raw >> 16) as u8,
            lowest: (raw >> 24) as u8,
        }
    }

    /// The request for the highest level only, with the energy preference at performance (0) and
    /// no desired level, so the hardware stays at the top of the range it is given.
    #[must_use]
    pub const fn request_highest(self) -> u64 {
        let top = self.highest as u64;
        top | top << 8
    }
}

/// How the clock is asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// Hardware P-states: the request holds a range of levels. Their capabilities can only be read
    /// once HWP is enabled, so each core reads its own after enabling it.
    Hwp { enabled_before: bool },
    /// Legacy P-states: one ratio in `IA32_PERF_CTL`.
    PerfCtl { ratio: Ratio },
}

/// What the processor allows and how the clock will be asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    pub method: Method,
    /// The base ratio from `MSR_PLATFORM_INFO`.
    pub base: Option<Ratio>,
    /// The single-core turbo ratio; `None` when the firmware has turned turbo off.
    pub turbo: Option<Ratio>,
    /// The ratio the boot processor runs at now, from `IA32_PERF_STATUS`.
    pub current: Option<Ratio>,
}

/// The ratio field, bits 15:8, of `MSR_PLATFORM_INFO`, `IA32_PERF_STATUS` and `IA32_PERF_CTL`.
#[must_use]
pub const fn ratio_field(raw: u64) -> Option<Ratio> {
    Ratio::new((raw >> 8) as u8)
}

/// The `IA32_PERF_CTL` value asking for `ratio`, keeping the bits of `old` outside the ratio field.
#[must_use]
pub const fn perf_ctl(old: u64, ratio: Ratio) -> u64 {
    old & !0xff00 | (ratio.0 as u64) << 8
}

/// Whether CPUID's family and model are a Core-line Intel processor this module knows the MSRs of.
#[must_use]
pub fn known_model(family: u32, model: u32) -> bool {
    family == 6 && model >= FIRST_MODEL && !ATOMS.contains(&model)
}

impl Plan {
    /// Read the processor's limits on the boot processor; `None`, with the reason logged, where this
    /// module does not know the processor's MSRs.
    #[must_use]
    pub fn probe() -> Option<Self> {
        let why = refuse();
        if let Some(why) = why {
            log::info!("cpu: clock left as the firmware set it: {why}");
            return None;
        }
        let hwp = __cpuid(6).eax & (1 << 7) != 0;
        // SAFETY: `refuse` passed: a Core-line Intel model from Sandy Bridge on, outside a hypervisor,
        // which has these MSRs.
        let (platform, misc, turbo_limit, status) = unsafe {
            (
                read_msr(MSR_PLATFORM_INFO),
                read_msr(IA32_MISC_ENABLE),
                read_msr(MSR_TURBO_RATIO_LIMIT),
                read_msr(IA32_PERF_STATUS),
            )
        };
        let base = ratio_field(platform);
        let turbo = if misc & TURBO_DISABLE == 0 { Ratio::new(turbo_limit as u8) } else { None };
        let current = ratio_field(status);
        let method = if hwp {
            // SAFETY: CPUID 6 EAX bit 7 says `IA32_PM_ENABLE` exists; the other HWP MSRs fault until it is set.
            let enabled = unsafe { read_msr(IA32_PM_ENABLE) };
            Method::Hwp { enabled_before: enabled & 1 != 0 }
        } else {
            let ratio = turbo.or(base)?;
            Method::PerfCtl { ratio }
        };
        let plan = Self { method, base, turbo, current };
        log::info!("cpu: {plan}");
        Some(plan)
    }

    /// Ask every core for the highest clock: the boot processor here, each AP of `mp` through a
    /// short task. Returns how many cores took the request.
    pub fn raise(&self, mp: Option<&Mp>) -> usize {
        let here = Raise::new(*self);
        here.run();
        let mut took = usize::from(here.took());
        if let Some(mp) = mp {
            let aps: alloc::vec::Vec<ProcessorNumber> = mp.aps().map(|p| p.number).collect();
            let tasks: alloc::vec::Vec<Raise> = aps.iter().map(|_| Raise::new(*self)).collect();
            mp.scope(|s| {
                for (&number, task) in aps.iter().zip(&tasks) {
                    if let Err(e) = s.start(number, task, AP_TIMEOUT) {
                        log::warn!("cpu: raising the clock on {number}: {e}");
                    }
                }
            });
            took += tasks.iter().filter(|t| t.took()).count();
            if let Some(task) = tasks.iter().find(|t| !t.took()) {
                log::warn!("cpu: a core did not take the clock request (read back {:#x})", task.readback());
            }
        }
        match self.method {
            Method::Hwp { .. } => {
                let l = HwpLevels::decode(here.capabilities());
                log::info!(
                    "cpu: asked {took} cores for the highest HWP level {} (guaranteed {}, efficient {}, lowest {}); the boot processor reads back {:#x}",
                    l.highest,
                    l.guaranteed,
                    l.efficient,
                    l.lowest,
                    here.readback()
                );
            },
            Method::PerfCtl { ratio } => {
                log::info!("cpu: asked {took} cores for {ratio}; the boot processor reads back {:#x}", here.readback());
            },
        }
        took
    }
}

/// Why this module leaves the clock alone on this processor, if it does.
fn refuse() -> Option<&'static str> {
    let vendor = __cpuid(0);
    if (vendor.ebx, vendor.edx, vendor.ecx) != (0x756e_6547, 0x4965_6e69, 0x6c65_746e) {
        return Some("not an Intel processor");
    }
    let leaf1 = __cpuid(1);
    if leaf1.ecx & (1 << 31) != 0 {
        return Some("running under a hypervisor");
    }
    if leaf1.ecx & (1 << 7) == 0 {
        return Some("no Enhanced SpeedStep (CPUID 1 ECX bit 7)");
    }
    let base_family = (leaf1.eax >> 8) & 0xf;
    let model = (leaf1.eax >> 4) & 0xf | ((leaf1.eax >> 16) & 0xf) << 4;
    if !known_model(base_family, model) || vendor.eax < 6 {
        return Some("a model whose ratio MSRs are not known here");
    }
    None
}

impl fmt::Display for Plan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let show = |r: Option<Ratio>| r.map_or(0, Ratio::mhz);
        write!(f, "clock now {} MHz, base {} MHz, ", show(self.current), show(self.base))?;
        match self.turbo {
            Some(t) => write!(f, "turbo up to {t}")?,
            None => f.write_str("turbo off in the firmware")?,
        }
        match self.method {
            Method::Hwp { enabled_before } => {
                write!(f, "; HWP, {}", if enabled_before { "already enabled" } else { "not yet enabled" })
            },
            Method::PerfCtl { ratio } => write!(f, "; legacy P-states, asking for {ratio}"),
        }
    }
}

/// The request on one core, with what it read back.
struct Raise {
    plan: Plan,
    readback: AtomicU64,
    /// `IA32_HWP_CAPABILITIES` as the core read it.
    capabilities: AtomicU64,
    took: AtomicBool,
}

impl Raise {
    const fn new(plan: Plan) -> Self {
        Self { plan, readback: AtomicU64::new(0), capabilities: AtomicU64::new(0), took: AtomicBool::new(false) }
    }

    fn took(&self) -> bool {
        self.took.load(Ordering::Acquire)
    }

    fn readback(&self) -> u64 {
        self.readback.load(Ordering::Relaxed)
    }

    fn capabilities(&self) -> u64 {
        self.capabilities.load(Ordering::Relaxed)
    }
}

// SAFETY: writes and reads the performance MSRs `Plan::probe` found on the boot processor, which every
// core of the package shares, and stores to atomics.
unsafe impl ApTask for Raise {
    fn run(&self) {
        // SAFETY: the plan came from `Plan::probe`, whose CPUID checks hold on every core.
        let (wanted, read) = unsafe {
            match self.plan.method {
                Method::Hwp { .. } => {
                    write_msr(IA32_PM_ENABLE, 1);
                    let caps = read_msr(IA32_HWP_CAPABILITIES);
                    self.capabilities.store(caps, Ordering::Relaxed);
                    let wanted = HwpLevels::decode(caps).request_highest();
                    write_msr(IA32_HWP_REQUEST, wanted);
                    (wanted, read_msr(IA32_HWP_REQUEST))
                },
                Method::PerfCtl { ratio } => {
                    let wanted = perf_ctl(read_msr(IA32_PERF_CTL), ratio);
                    write_msr(IA32_PERF_CTL, wanted);
                    (wanted, read_msr(IA32_PERF_CTL))
                },
            }
        };
        self.readback.store(read, Ordering::Relaxed);
        self.took.store(read & 0xffff_ffff == wanted & 0xffff_ffff, Ordering::Release);
    }

    fn finish(&self, _stack: StackUse) {}
}

#[cfg(test)]
mod tests;
