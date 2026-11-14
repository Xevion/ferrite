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
//! Nothing here can recover from a fault, and an MSR the CPU lacks raises one. Every MSR read is
//! therefore gated on the CPUID bits and model that say the register exists, and all of them are
//! skipped under a hypervisor, whose MSR set is its own. The hardware decode is unverified on
//! anything but the machines in the project's hardware notes; the pure decoders are tested against
//! register values.

use alloc::string::String;
use core::arch::x86_64::{__cpuid, __cpuid_count};
use core::fmt;
use core::time::Duration;

use ferrite_hal::{Instant, PciAddress, PciConfig};
use uefi::boot;
use uefi::proto::pci::root_bridge::PciRootBridgeIo;

use crate::clock::Clock;
use crate::paging::read_msr;
use crate::pci::RootBridge;

const IA32_MPERF: u32 = 0xe7;
const IA32_APERF: u32 = 0xe8;
const IA32_THERM_STATUS: u32 = 0x19c;
const MSR_TEMPERATURE_TARGET: u32 = 0x1a2;
const IA32_PACKAGE_THERM_STATUS: u32 = 0x1b1;

/// The root complex's SMN index and data registers in PCI configuration space, and the register
/// that holds the current control temperature.
const SMN_INDEX: u8 = 0x60;
const SMN_DATA: u8 = 0x64;
const THM_TCON_CUR_TMP: u32 = 0x0005_9800;
const AMD_PCI_VENDOR: u16 = 0x1022;

/// Samples closer together than this give the last reading again: a short interval measures noise.
const MIN_INTERVAL: Duration = Duration::from_millis(100);

/// Who made the CPU, from CPUID leaf 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vendor {
    Intel,
    Amd,
    /// The twelve bytes of any other vendor string.
    Other([u8; 12]),
}

impl Vendor {
    /// The vendor string is in EBX, EDX, ECX order.
    fn from_registers(ebx: u32, ecx: u32, edx: u32) -> Self {
        let mut text = [0; 12];
        for (byte, value) in text.iter_mut().zip([ebx, edx, ecx].into_iter().flat_map(u32::to_le_bytes)) {
            *byte = value;
        }
        match &text {
            b"GenuineIntel" => Self::Intel,
            b"AuthenticAMD" => Self::Amd,
            _ => Self::Other(text),
        }
    }
}

impl fmt::Display for Vendor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Intel => f.write_str("GenuineIntel"),
            Self::Amd => f.write_str("AuthenticAMD"),
            Self::Other(text) => {
                let end = text.iter().position(|&b| b == 0).unwrap_or(text.len());
                f.write_str(core::str::from_utf8(&text[..end]).unwrap_or("unknown"))
            },
        }
    }
}

/// Family, model and stepping with the extended fields of CPUID leaf 1 folded in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Signature {
    pub family: u16,
    pub model: u8,
    pub stepping: u8,
}

impl Signature {
    /// The signature in leaf 1's EAX: the extended family adds to a base family of 0xf, and the
    /// extended model is the model's high nibble for families 6 and 0xf.
    #[must_use]
    pub const fn from_eax(eax: u32) -> Self {
        let base_family = (eax >> 8) as u16 & 0xf;
        let family = if base_family == 0xf { base_family + ((eax >> 20) as u16 & 0xff) } else { base_family };
        let model = (eax >> 4) as u8 & 0xf;
        let model =
            if base_family == 0x6 || base_family == 0xf { model | ((eax >> 16) as u8 & 0xf) << 4 } else { model };
        Self { family, model, stepping: eax as u8 & 0xf }
    }
}

impl fmt::Display for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "family {} model {:#x} stepping {}", self.family, self.model, self.stepping)
    }
}

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
        let mut leaves = [[0_u32; 4]; 3];
        if __cpuid(0x8000_0000).eax >= 0x8000_0004 {
            for (leaf, regs) in (0x8000_0002..).zip(&mut leaves) {
                let r = __cpuid(leaf);
                *regs = [r.eax, r.ebx, r.ecx, r.edx];
            }
        }
        Self {
            vendor: Vendor::from_registers(leaf0.ebx, leaf0.ecx, leaf0.edx),
            signature: Signature::from_eax(__cpuid(1).eax),
            brand: brand_string(&leaves),
        }
    }
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

/// The CPUID bits that say which registers exist.
#[expect(clippy::struct_excessive_bools, reason = "one flag for each CPUID bit, read together")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Features {
    hypervisor: bool,
    /// Leaf 6 EAX bit 0: the digital thermal sensor.
    digital_sensor: bool,
    /// Leaf 6 EAX bit 6: the package thermal management registers.
    package_sensor: bool,
    /// Leaf 6 ECX bit 0: `IA32_APERF` and `IA32_MPERF`.
    aperf_mperf: bool,
    /// Leaf 0x16 EAX: the base frequency in MHz.
    base_mhz: Option<u32>,
}

impl Features {
    fn read() -> Self {
        let max = __cpuid(0).eax;
        let thermal = (max >= 6).then(|| __cpuid(6));
        let frequency = (max >= 0x16).then(|| __cpuid_count(0x16, 0).eax);
        Self::from_leaves(__cpuid(1).ecx, thermal.map(|r| (r.eax, r.ecx)), frequency)
    }

    const fn from_leaves(features_ecx: u32, thermal: Option<(u32, u32)>, frequency_eax: Option<u32>) -> Self {
        let (sensor_bits, counter_bits) = match thermal {
            Some(regs) => regs,
            None => (0, 0),
        };
        let base = match frequency_eax {
            Some(eax) => eax & 0xffff,
            None => 0,
        };
        Self {
            hypervisor: features_ecx & (1 << 31) != 0,
            digital_sensor: sensor_bits & 1 != 0,
            package_sensor: sensor_bits & (1 << 6) != 0,
            aperf_mperf: counter_bits & 1 != 0,
            base_mhz: if base == 0 { None } else { Some(base) },
        }
    }
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

fn plan(vendor: Vendor, sig: Signature, features: &Features) -> Result<Plan, &'static str> {
    if features.hypervisor {
        return Err("running under a hypervisor");
    }
    match vendor {
        Vendor::Intel if !features.digital_sensor => Err("CPUID reports no digital thermal sensor"),
        Vendor::Intel if !has_temperature_target(sig) => Err("no TjMax register on this model"),
        Vendor::Intel => Ok(Plan::IntelDigital { package: features.package_sensor }),
        Vendor::Amd if sig.family >= 0x17 => Ok(Plan::AmdSmn),
        Vendor::Amd => Err("no SMN thermal register before AMD family 17h"),
        Vendor::Other(_) => Err("no known sensor for this vendor"),
    }
}

/// `TjMax`, the temperature the digital readout counts down from: bits 23:16 of `MSR_TEMPERATURE_TARGET`,
/// 0 when the register has none.
const fn tj_max(target: u64) -> Option<u8> {
    match (target >> 16) as u8 {
        0 => None,
        tj => Some(tj),
    }
}

/// Degrees Celsius from a thermal status register: the readout in bits 22:16 is degrees below `TjMax`,
/// good only while the valid bit (31) is set.
fn digital_celsius(status: u64, tj_max: u8) -> Option<i16> {
    if status & (1 << 31) == 0 {
        return None;
    }
    Some(i16::from(tj_max) - i16::from((status >> 16 & 0x7f) as u8))
}

/// Degrees Celsius from `THM_TCON_CUR_TMP`: bits 31:21 in eighths of a degree, less 49 degrees when the
/// range-select bit (19) is set. All ones is what a read of nothing returns. This is the control
/// temperature (Tctl), which some parts offset from the die temperature.
fn smn_celsius(raw: u32) -> Option<i16> {
    if raw == u32::MAX {
        return None;
    }
    let eighths = i32::try_from(raw >> 21).ok()?;
    let milli = eighths * 125 - if raw & (1 << 19) != 0 { 49_000 } else { 0 };
    i16::try_from((milli + 500).div_euclid(1000)).ok()
}

/// The clock in MHz from the TSC frequency and how far APERF and MPERF advanced; `None` when MPERF
/// did not move (the core never ran) or the result is zero.
fn effective_mhz(tsc_hz: u64, aperf: u64, mperf: u64) -> Option<u32> {
    if mperf == 0 {
        return None;
    }
    let mhz = u128::from(tsc_hz) * u128::from(aperf) / u128::from(mperf) / 1_000_000;
    u32::try_from(mhz).ok().filter(|&mhz| mhz > 0)
}

/// A clock figure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frequency {
    pub mhz: u32,
    /// Measured over an interval, rather than the base frequency CPUID states.
    pub measured: bool,
}

/// Where a temperature was read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
        smn_celsius(self.read(THM_TCON_CUR_TMP)?)
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
    tsc_hz: u64,
    thermal: Option<Thermal>,
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
        let features = Features::read();
        let tsc_hz = clock.tsc_hz();
        log::info!("cpu: {identity}");
        log::info!(
            "cpu: TSC {} MHz measured, base {} by CPUID 0x16, APERF/MPERF {}",
            tsc_hz / 1_000_000,
            features.base_mhz.map_or_else(|| String::from("not given"), |mhz| alloc::format!("{mhz} MHz")),
            if features.aperf_mperf && !features.hypervisor { "read" } else { "not read" },
        );
        let thermal = match plan(identity.vendor, identity.signature, &features) {
            Ok(plan) => Self::open_thermal(plan),
            Err(why) => {
                log::info!("cpu: no temperature: {why}");
                None
            },
        };
        let mut sensors = Self {
            identity,
            features,
            tsc_hz,
            thermal,
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

    fn open_thermal(plan: Plan) -> Option<Thermal> {
        match plan {
            Plan::IntelDigital { package } => {
                // SAFETY: `plan` allowed this on an Intel family 6 model that has `MSR_TEMPERATURE_TARGET`.
                let target = unsafe { read_msr(MSR_TEMPERATURE_TARGET) };
                let Some(tj_max) = tj_max(target) else {
                    log::info!("cpu: no temperature: MSR_TEMPERATURE_TARGET gives no TjMax ({target:#x})");
                    return None;
                };
                log::info!("cpu: TjMax {tj_max} C, {} sensor", if package { "package" } else { "core" });
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
        let base = self.features.base_mhz.map(|mhz| Frequency { mhz, measured: false });
        self.reading.frequency = self.measure_frequency().or(base);
        self.reading.temperature = self.read_temperature();
        self.reading
    }

    /// The effective clock over the interval since the last call, once there is one.
    fn measure_frequency(&mut self) -> Option<Frequency> {
        if !self.features.aperf_mperf || self.features.hypervisor {
            return None;
        }
        // SAFETY: CPUID leaf 6 reports APERF and MPERF, and no hypervisor is hiding them.
        let now = unsafe { Counters { aperf: read_msr(IA32_APERF), mperf: read_msr(IA32_MPERF) } };
        let before = self.counters.replace(now)?;
        effective_mhz(self.tsc_hz, now.aperf.wrapping_sub(before.aperf), now.mperf.wrapping_sub(before.mperf))
            .map(|mhz| Frequency { mhz, measured: true })
    }

    fn read_temperature(&mut self) -> Option<Temperature> {
        match self.thermal.as_mut()? {
            Thermal::Intel { tj_max, package } => {
                let (tj_max, package) = (*tj_max, *package);
                let package = package.then(|| {
                    // SAFETY: leaf 6 reports the package thermal registers.
                    let status = unsafe { read_msr(IA32_PACKAGE_THERM_STATUS) };
                    digital_celsius(status, tj_max).map(|c| (c, TempSource::IntelPackage))
                });
                let found = package.flatten().or_else(|| {
                    // SAFETY: the digital sensor bit is set, so IA32_THERM_STATUS exists.
                    let status = unsafe { read_msr(IA32_THERM_STATUS) };
                    digital_celsius(status, tj_max).map(|c| (c, TempSource::IntelCore))
                });
                found.map(|(celsius, source)| Temperature { celsius, source })
            },
            Thermal::Amd(smn) => smn.celsius().map(|celsius| Temperature { celsius, source: TempSource::AmdSmn }),
        }
    }
}

#[cfg(test)]
mod tests;
