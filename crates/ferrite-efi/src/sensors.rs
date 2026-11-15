//! The processor's identity, its clock and its temperature, read on the boot processor.
//!
//! The clock is the effective frequency over the interval between two [`Sensors::sample`] calls:
//! `IA32_APERF` counts at the running frequency and `IA32_MPERF` at the TSC's, so the TSC frequency scaled
//! by their ratio is the average clock while the core ran. A CPU without the pair reports the base
//! frequency of CPUID leaf 0x16 instead, marked as not measured.
//!
//! The temperature comes from the digital thermal sensor on Intel (package when CPUID says there is
//! one, else the core) and from the SMN thermal register on AMD family 17h and later, which is reached
//! through the index and data pair in the root complex's PCI configuration space. An unsupported
//! sensor reads as `None`; nothing is guessed.
//!
//! The registers are read without fault handling, so every MSR read is gated on the CPUID bits and
//! model that say the register exists, and all of them are skipped under a hypervisor, whose MSR
//! set is its own. The decoders are `ferrite_power`'s.

use alloc::string::String;
use core::arch::x86_64::{__cpuid, __cpuid_count};
use core::fmt;
use core::time::Duration;

use ferrite_hal::{Instant, PciAddress, PciConfig};
use ferrite_power::amd::tctl_celsius;
use ferrite_power::cpuid::{Feature, Features, Signature, Vendor, read_features};
use ferrite_power::intel;
use ferrite_power::msr::MsrIo;
use ferrite_power::units::{Mhz, effective_mhz};
use uefi::boot;
use uefi::proto::pci::root_bridge::PciRootBridgeIo;

use crate::clock::Clock;
use crate::pci::RootBridge;
use crate::x86::{Cpuid, RawMsrs};

/// The root complex's SMN index and data registers in PCI configuration space, and the register
/// that holds the current control temperature.
const SMN_INDEX: u8 = 0x60;
const SMN_DATA: u8 = 0x64;
const THM_TCON_CUR_TMP: u32 = 0x0005_9800;
const AMD_PCI_VENDOR: u16 = 0x1022;

/// Samples closer together than this give the last reading again: a short interval measures noise.
const MIN_INTERVAL: Duration = Duration::from_millis(100);

/// What CPUID says the processor is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub vendor: Vendor,
    pub signature: Signature,
    /// The brand string, trimmed and with its runs of spaces made one; empty without leaves
    /// 0x80000002 to 0x80000004.
    pub brand: String,
}

impl Identity {
    #[must_use]
    pub fn read() -> Self {
        let leaf0 = __cpuid(0);
        Self {
            vendor: Vendor::from_registers(leaf0.ebx, leaf0.ecx, leaf0.edx),
            signature: Signature::from_eax(__cpuid(1).eax),
            brand: brand_string(&brand_leaves()),
        }
    }
}

/// The registers of leaves 0x80000002 to 0x80000004, all zero when the processor has no brand string.
fn brand_leaves() -> [[u32; 4]; 3] {
    let mut leaves = [[0_u32; 4]; 3];
    if __cpuid(0x8000_0000).eax >= 0x8000_0004 {
        for (leaf, regs) in (0x8000_0002..).zip(&mut leaves) {
            let r = __cpuid(leaf);
            *regs = [r.eax, r.ebx, r.ecx, r.edx];
        }
    }
    leaves
}

/// The raw identifying bytes of the processor, untrimmed.
///
/// The 48 bytes of the brand string leaves (zeros without them), then leaf 1 EAX, little-endian.
/// Machine ids are hashes of these bytes, so their layout must not change.
#[must_use]
pub fn fingerprint() -> [u8; 52] {
    fingerprint_of(&brand_leaves(), __cpuid(1).eax)
}

fn fingerprint_of(leaves: &[[u32; 4]; 3], leaf1_eax: u32) -> [u8; 52] {
    let mut out = [0_u8; 52];
    for (chunk, reg) in out.as_chunks_mut::<4>().0.iter_mut().zip(leaves.iter().flatten().chain([&leaf1_eax])) {
        chunk.copy_from_slice(&reg.to_le_bytes());
    }
    out
}

impl fmt::Display for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}, \"{}\"", self.vendor, self.signature, self.brand)
    }
}

/// The brand string held in the registers of leaves 0x80000002 to 0x80000004, cut at the first NUL.
fn brand_string(leaves: &[[u32; 4]; 3]) -> String {
    let bytes: alloc::vec::Vec<u8> =
        leaves.iter().flatten().flat_map(|reg| reg.to_le_bytes()).take_while(|&b| b != 0).collect();
    let text = String::from_utf8_lossy(&bytes);
    text.split_whitespace().collect::<alloc::vec::Vec<_>>().join(" ")
}

/// Which sensor to read the temperature from, or why there is none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Plan {
    IntelDigital { package: bool },
    AmdSmn,
}

/// Whether the model has `MSR_TEMPERATURE_TARGET`: Nehalem and later, without the first Atoms.
const fn has_temperature_target(sig: Signature) -> bool {
    sig.family == 6 && sig.model >= 0x1a && !matches!(sig.model, 0x1c | 0x26 | 0x27 | 0x35 | 0x36)
}

fn plan(vendor: Vendor, sig: Signature, features: Features) -> Result<Plan, &'static str> {
    if features.has(Feature::Hypervisor) {
        return Err("running under a hypervisor");
    }
    match vendor {
        Vendor::Intel if !features.has(Feature::DigitalSensor) => Err("CPUID reports no digital thermal sensor"),
        Vendor::Intel if !has_temperature_target(sig) => Err("no TjMax register on this model"),
        Vendor::Intel => Ok(Plan::IntelDigital { package: features.has(Feature::PackageThermal) }),
        Vendor::Amd if sig.family >= 0x17 => Ok(Plan::AmdSmn),
        Vendor::Amd => Err("no SMN thermal register before AMD family 17h"),
        Vendor::Other(_) => Err("no known sensor for this vendor"),
    }
}

/// A clock figure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frequency {
    pub mhz: Mhz,
    /// Measured over an interval, rather than the base frequency CPUID states.
    pub measured: bool,
}

/// Where a temperature was read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::IntoStaticStr)]
#[strum(serialize_all = "kebab-case")]
pub enum TempSource {
    IntelPackage,
    IntelCore,
    AmdSmn,
}

impl fmt::Display for TempSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::IntelPackage => "the Intel package thermal status",
            Self::IntelCore => "the Intel core thermal status",
            Self::AmdSmn => "the AMD SMN thermal register",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Temperature {
    pub celsius: i16,
    pub source: TempSource,
}

/// The clock and the temperature as last sampled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reading {
    pub frequency: Option<Frequency>,
    pub temperature: Option<Temperature>,
}

/// The APERF and MPERF counts at one sample.
#[derive(Clone, Copy, Debug)]
struct Counters {
    aperf: u64,
    mperf: u64,
}

/// SMN access through the root complex of an AMD processor.
struct Smn {
    bridge: RootBridge,
    root: PciAddress,
}

impl Smn {
    /// The root bridge whose device 0:0.0 is AMD's root complex, if there is one.
    fn open() -> Option<Self> {
        let handles = boot::find_handles::<PciRootBridgeIo>().ok()?;
        handles.into_iter().find_map(|handle| {
            let mut bridge = RootBridge::open(handle).ok()?;
            let root = PciAddress::checked(bridge.segment(), 0, 0, 0)?;
            (bridge.cfg_read16(root, 0).ok()? == AMD_PCI_VENDOR).then_some(Self { bridge, root })
        })
    }

    /// The SMN register at `address`: write it to the index register, read the data register.
    fn read(&mut self, address: u32) -> Option<u32> {
        self.bridge.cfg_write32(self.root, SMN_INDEX, address).ok()?;
        self.bridge.cfg_read32(self.root, SMN_DATA).ok()
    }

    fn celsius(&mut self) -> Option<i16> {
        tctl_celsius(self.read(THM_TCON_CUR_TMP)?)
    }
}

/// How the temperature is read, decided at start.
enum Thermal {
    Intel { tj_max: u8, package: bool },
    Amd(Smn),
}

/// The CPU's identity and its clock and temperature sensors. Sample from the boot processor: the
/// counters and the core thermal status are per core.
pub struct Sensors {
    identity: Identity,
    features: Features,
    /// The base frequency CPUID leaf 0x16 states.
    base_mhz: Option<u32>,
    tsc_hz: u64,
    thermal: Option<Thermal>,
    /// Every read is gated on the CPUID bits and model that say the register exists.
    msrs: RawMsrs,
    counters: Option<Counters>,
    last_sample: Option<Instant>,
    reading: Reading,
}

impl Sensors {
    /// Find which sensors this CPU has and log what was found: the identity, the clocks, and the
    /// temperature source with its first reading, or why there is none.
    #[must_use]
    pub fn probe(clock: &Clock) -> Self {
        let identity = Identity::read();
        let features = read_features(&Cpuid);
        let base_mhz = (__cpuid(0).eax >= 0x16).then(|| __cpuid_count(0x16, 0).eax & 0xffff).filter(|&mhz| mhz != 0);
        let tsc_hz = clock.tsc_hz();
        log::info!("cpu: {identity}");
        log::info!(
            "cpu: TSC {} MHz measured, base {} by CPUID 0x16, APERF/MPERF {}",
            tsc_hz / 1_000_000,
            base_mhz.map_or_else(|| String::from("not given"), |mhz| alloc::format!("{mhz} MHz")),
            if Self::counts(features) { "read" } else { "not read" },
        );
        // SAFETY: each read below is gated on the CPUID bits and model that say its register exists.
        let mut msrs = unsafe { RawMsrs::new() };
        let thermal = match plan(identity.vendor, identity.signature, features) {
            Ok(plan) => Self::open_thermal(plan, &mut msrs),
            Err(why) => {
                log::info!("cpu: no temperature: {why}");
                None
            },
        };
        let mut sensors = Self {
            identity,
            features,
            base_mhz,
            tsc_hz,
            thermal,
            msrs,
            counters: None,
            last_sample: None,
            reading: Reading::default(),
        };
        if sensors.thermal.is_some() {
            match sensors.read_temperature() {
                Some(t) => log::info!("cpu: temperature from {}, {} C now", t.source, t.celsius),
                None => log::warn!("cpu: the temperature sensor gave no reading"),
            }
        }
        sensors
    }

    fn counts(features: Features) -> bool {
        features.has(Feature::AperfMperf) && !features.has(Feature::Hypervisor)
    }

    fn open_thermal(plan: Plan, msrs: &mut RawMsrs) -> Option<Thermal> {
        match plan {
            Plan::IntelDigital { package } => {
                // `plan` allowed this on an Intel family 6 model that has `MSR_TEMPERATURE_TARGET`.
                let target = msrs.read(intel::TEMPERATURE_TARGET).ok()?;
                let tj_max = target.tj_max();
                if tj_max == 0 {
                    log::info!(
                        "cpu: no temperature: MSR_TEMPERATURE_TARGET gives no TjMax ({:#x})",
                        target.raw_value()
                    );
                    return None;
                }
                log::info!(
                    "cpu: TjMax {tj_max} C, throttling from {} C, {} sensor",
                    target.throttle_celsius().unwrap_or_else(|| i16::from(tj_max)),
                    if package { "package" } else { "core" }
                );
                Some(Thermal::Intel { tj_max, package })
            },
            Plan::AmdSmn => {
                let smn = Smn::open();
                if smn.is_none() {
                    log::info!("cpu: no temperature: no AMD root complex at 00:00.0 to read the SMN through");
                }
                smn.map(Thermal::Amd)
            },
        }
    }

    #[must_use]
    pub const fn identity(&self) -> &Identity {
        &self.identity
    }

    /// Read the sensors as of `now`. The clock is the average over the time since the previous call,
    /// and a call within a tenth of a second of it returns the reading it had.
    pub fn sample(&mut self, now: Instant) -> Reading {
        if self.last_sample.is_some_and(|last| now - last < MIN_INTERVAL) {
            return self.reading;
        }
        self.last_sample = Some(now);
        let base = self.base_mhz.map(|mhz| Frequency { mhz: Mhz(mhz), measured: false });
        self.reading.frequency = self.measure_frequency().or(base);
        self.reading.temperature = self.read_temperature();
        self.reading
    }

    /// The effective clock over the interval since the last call, once there is one.
    fn measure_frequency(&mut self) -> Option<Frequency> {
        if !Self::counts(self.features) {
            return None;
        }
        let now =
            Counters { aperf: self.msrs.read(intel::IA32_APERF).ok()?, mperf: self.msrs.read(intel::IA32_MPERF).ok()? };
        let before = self.counters.replace(now)?;
        effective_mhz(self.tsc_hz, now.aperf.wrapping_sub(before.aperf), now.mperf.wrapping_sub(before.mperf))
            .map(|mhz| Frequency { mhz, measured: true })
    }

    fn read_temperature(&mut self) -> Option<Temperature> {
        match self.thermal.as_mut()? {
            Thermal::Intel { tj_max, package } => {
                let (tj_max, package) = (*tj_max, *package);
                let msrs = &mut self.msrs;
                let package = package
                    .then(|| msrs.read(intel::IA32_PACKAGE_THERM_STATUS).ok()?.celsius(tj_max))
                    .flatten()
                    .map(|c| (c, TempSource::IntelPackage));
                let found = package.or_else(|| {
                    msrs.read(intel::IA32_THERM_STATUS).ok()?.celsius(tj_max).map(|c| (c, TempSource::IntelCore))
                });
                found.map(|(celsius, source)| Temperature { celsius, source })
            },
            Thermal::Amd(smn) => smn.celsius().map(|celsius| Temperature { celsius, source: TempSource::AmdSmn }),
        }
    }
}

#[cfg(test)]
mod tests;
