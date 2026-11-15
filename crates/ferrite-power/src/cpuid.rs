//! What CPUID says about the processor's identity and its power-management features.

use core::fmt;

use bitbybit::{bitenum, bitfield};

use ferrite_hal::Set;

/// One CPUID result.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Leaf {
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
}

/// CPUID on the core that calls it.
pub trait CpuidSource {
    fn cpuid(&self, leaf: u32, subleaf: u32) -> Leaf;

    /// `leaf`, or zeros when it is past the maximum its range reports.
    fn cpuid_checked(&self, leaf: u32) -> Leaf {
        let max = self.cpuid(leaf & 0x8000_0000, 0).eax;
        if leaf <= max { self.cpuid(leaf, 0) } else { Leaf::default() }
    }
}

impl<F: Fn(u32, u32) -> Leaf> CpuidSource for F {
    fn cpuid(&self, leaf: u32, subleaf: u32) -> Leaf {
        self(leaf, subleaf)
    }
}

/// Leaf 1 ECX.
#[bitfield(u32, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct Leaf1Ecx {
    #[bit(7, r)]
    eist: bool,
    #[bit(31, r)]
    hypervisor: bool,
}

/// Leaf 6 EAX: thermal and power management.
#[bitfield(u32, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct Leaf6Eax {
    #[bit(0, r)]
    digital_sensor: bool,
    #[bit(1, r)]
    turbo_boost: bool,
    #[bit(4, r)]
    power_limit_notify: bool,
    #[bit(6, r)]
    package_thermal: bool,
    #[bit(7, r)]
    hwp: bool,
    #[bit(8, r)]
    hwp_notify: bool,
    #[bit(9, r)]
    hwp_activity_window: bool,
    #[bit(10, r)]
    hwp_epp: bool,
    #[bit(11, r)]
    hwp_package_request: bool,
}

/// Leaf 6 ECX.
#[bitfield(u32, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct Leaf6Ecx {
    #[bit(0, r)]
    aperf_mperf: bool,
    #[bit(3, r)]
    epb: bool,
}

/// Leaf 7 subleaf 0 EDX.
#[bitfield(u32, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct Leaf7Edx {
    #[bit(15, r)]
    hybrid: bool,
}

/// Leaf `0x8000_0007` EDX: advanced power management.
#[bitfield(u32, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct PowerEdx {
    #[bit(7, r)]
    hw_pstate: bool,
    #[bit(8, r)]
    invariant_tsc: bool,
    #[bit(9, r)]
    cpb: bool,
    #[bit(14, r)]
    rapl: bool,
}

/// Leaf `0x8000_0008` EBX.
#[bitfield(u32, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct Leaf8Ebx {
    #[bit(27, r)]
    cppc: bool,
}

/// Leaf 0x1A EAX on a hybrid part, read on the core it describes.
#[bitfield(u32, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct HybridEax {
    #[bits(24..=31, r)]
    core_type: Option<CoreType>,
}

/// The kind of core on a hybrid Intel part.
#[bitenum(u8, exhaustive = false)]
#[derive(Debug, PartialEq, Eq, strum::IntoStaticStr)]
#[strum(serialize_all = "kebab-case")]
pub enum CoreType {
    /// An E-core.
    Efficient = 0x20,
    /// A P-core.
    Performance = 0x40,
}

impl CoreType {
    /// This core's type; `None` on a part that is not hybrid or a type this crate does not know.
    #[must_use]
    pub fn read(source: &impl CpuidSource, features: Features) -> Option<Self> {
        if !features.has(Feature::Hybrid) {
            return None;
        }
        HybridEax::new_with_raw_value(source.cpuid_checked(0x1a).eax).core_type().ok()
    }
}

/// Who made the CPU, from CPUID leaf 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vendor {
    Intel,
    /// AMD, and Hygon, whose Dhyana parts are AMD family 17h under another name.
    Amd,
    /// The twelve bytes of any other vendor string.
    Other([u8; 12]),
}

impl Vendor {
    /// The vendor string is in EBX, EDX, ECX order.
    #[must_use]
    pub fn from_registers(ebx: u32, ecx: u32, edx: u32) -> Self {
        let mut text = [0; 12];
        for (byte, value) in text.iter_mut().zip([ebx, edx, ecx].into_iter().flat_map(u32::to_le_bytes)) {
            *byte = value;
        }
        match &text {
            b"GenuineIntel" => Self::Intel,
            b"AuthenticAMD" | b"HygonGenuine" => Self::Amd,
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

/// A CPUID bit that says a power-management register or behaviour exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::IntoStaticStr, strum::VariantArray)]
#[strum(serialize_all = "snake_case")]
pub enum Feature {
    /// A hypervisor runs us, and its MSRs are its own.
    Hypervisor,
    /// Enhanced `SpeedStep`: `IA32_PERF_CTL`.
    Eist,
    /// `IA32_THERM_STATUS`.
    DigitalSensor,
    TurboBoost,
    /// Power limit notification in the thermal status registers.
    PowerLimitNotify,
    /// `IA32_PACKAGE_THERM_STATUS`.
    PackageThermal,
    /// `IA32_PM_ENABLE` and the HWP registers.
    Hwp,
    HwpNotify,
    HwpActivityWindow,
    /// The energy-performance preference field of `IA32_HWP_REQUEST`.
    HwpEpp,
    /// `IA32_HWP_REQUEST_PKG`.
    HwpPackageRequest,
    /// `IA32_APERF` and `IA32_MPERF`.
    AperfMperf,
    /// `IA32_ENERGY_PERF_BIAS`.
    Epb,
    /// Performance and efficient cores of different kinds.
    Hybrid,
    /// AMD's `PStateDef` registers.
    AmdHwPstate,
    /// AMD Core Performance Boost.
    AmdCpb,
    /// AMD's RAPL energy counters.
    AmdRapl,
    /// AMD CPPC through MSRs.
    AmdCppc,
    InvariantTsc,
}

ferrite_hal::flag!(Feature);

/// The set of [`Feature`]s CPUID reports.
pub type Features = Set<Feature>;

/// The features in the leaves that exist; a leaf past its range's maximum reads as zero.
#[must_use]
pub fn read_features(source: &impl CpuidSource) -> Features {
    let l1 = Leaf1Ecx::new_with_raw_value(source.cpuid_checked(1).ecx);
    let l6 = source.cpuid_checked(6);
    let (l6a, l6c) = (Leaf6Eax::new_with_raw_value(l6.eax), Leaf6Ecx::new_with_raw_value(l6.ecx));
    let l7 = Leaf7Edx::new_with_raw_value(if source.cpuid(0, 0).eax >= 7 { source.cpuid(7, 0).edx } else { 0 });
    let power = PowerEdx::new_with_raw_value(source.cpuid_checked(0x8000_0007).edx);
    let l8 = Leaf8Ebx::new_with_raw_value(source.cpuid_checked(0x8000_0008).ebx);
    [
        (Feature::Hypervisor, l1.hypervisor()),
        (Feature::Eist, l1.eist()),
        (Feature::DigitalSensor, l6a.digital_sensor()),
        (Feature::TurboBoost, l6a.turbo_boost()),
        (Feature::PowerLimitNotify, l6a.power_limit_notify()),
        (Feature::PackageThermal, l6a.package_thermal()),
        (Feature::Hwp, l6a.hwp()),
        (Feature::HwpNotify, l6a.hwp_notify()),
        (Feature::HwpActivityWindow, l6a.hwp_activity_window()),
        (Feature::HwpEpp, l6a.hwp_epp()),
        (Feature::HwpPackageRequest, l6a.hwp_package_request()),
        (Feature::AperfMperf, l6c.aperf_mperf()),
        (Feature::Epb, l6c.epb()),
        (Feature::Hybrid, l7.hybrid()),
        (Feature::AmdHwPstate, power.hw_pstate()),
        (Feature::AmdCpb, power.cpb()),
        (Feature::AmdRapl, power.rapl()),
        (Feature::AmdCppc, l8.cppc()),
        (Feature::InvariantTsc, power.invariant_tsc()),
    ]
    .into_iter()
    .filter_map(|(feature, set)| set.then_some(feature))
    .collect()
}

/// What CPUID says the processor is, as far as power management cares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cpu {
    pub vendor: Vendor,
    pub signature: Signature,
    pub features: Features,
}

impl Cpu {
    #[must_use]
    pub fn read(source: &impl CpuidSource) -> Self {
        let leaf0 = source.cpuid(0, 0);
        Self {
            vendor: Vendor::from_registers(leaf0.ebx, leaf0.ecx, leaf0.edx),
            signature: Signature::from_eax(source.cpuid_checked(1).eax),
            features: read_features(source),
        }
    }

    /// The model number when this is an Intel family 6 part.
    #[must_use]
    pub const fn intel_family6_model(&self) -> Option<u8> {
        match self.vendor {
            Vendor::Intel if self.signature.family == 6 => Some(self.signature.model),
            _ => None,
        }
    }
}

impl fmt::Display for Cpu {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}, features: {}", self.vendor, self.signature, self.features)
    }
}
