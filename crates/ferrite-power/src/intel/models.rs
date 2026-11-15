//! The Intel family 6 models this crate knows the power registers of, and what each one has.
//!
//! Models from Linux's `intel-family.h`; what each has from `intel_rapl_msr`, `intel_pstate` and
//! `turbostat`. Nehalem and older, and the first Atoms, are left out: their ratio and energy
//! registers differ.

use bitbybit::bitenum;

use crate::limits::Layout;
use crate::rapl::Flavor;

#[bitenum(u8, exhaustive = false)]
#[derive(Debug, PartialEq, Eq, strum::IntoStaticStr, strum::VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum Model {
    SandyBridge = 0x2a,
    SandyBridgeX = 0x2d,
    IvyBridge = 0x3a,
    IvyBridgeX = 0x3e,
    Haswell = 0x3c,
    HaswellX = 0x3f,
    HaswellL = 0x45,
    HaswellG = 0x46,
    Broadwell = 0x3d,
    BroadwellG = 0x47,
    BroadwellX = 0x4f,
    BroadwellD = 0x56,
    SkylakeL = 0x4e,
    Skylake = 0x5e,
    SkylakeX = 0x55,
    KabyLakeL = 0x8e,
    KabyLake = 0x9e,
    CometLake = 0xa5,
    CometLakeL = 0xa6,
    CannonLakeL = 0x66,
    IceLakeX = 0x6a,
    IceLakeD = 0x6c,
    IceLake = 0x7d,
    IceLakeL = 0x7e,
    RocketLake = 0xa7,
    TigerLakeL = 0x8c,
    TigerLake = 0x8d,
    SapphireRapidsX = 0x8f,
    EmeraldRapidsX = 0xcf,
    Lakefield = 0x8a,
    AlderLake = 0x97,
    AlderLakeL = 0x9a,
    RaptorLake = 0xb7,
    RaptorLakeP = 0xba,
    RaptorLakeS = 0xbf,
    BartlettLake = 0xd7,
    MeteorLake = 0xac,
    MeteorLakeL = 0xaa,
    ArrowLakeH = 0xc5,
    ArrowLake = 0xc6,
    ArrowLakeU = 0xb5,
    LunarLakeM = 0xbd,
    PantherLakeL = 0xcc,
    AtomSilvermont = 0x37,
    AtomSilvermontMid = 0x4a,
    AtomAirmont = 0x4c,
    AtomSilvermontMid2 = 0x5a,
    AtomGoldmont = 0x5c,
    AtomGoldmontD = 0x5f,
    AtomGoldmontPlus = 0x7a,
    AtomTremontD = 0x86,
    AtomTremont = 0x96,
    AtomTremontL = 0x9c,
    AtomGracemont = 0xbe,
    XeonPhiKnl = 0x57,
    XeonPhiKnm = 0x85,
}

impl Model {
    /// The model CPUID reports, if this crate knows it.
    #[must_use]
    pub fn of(model: u8) -> Option<Self> {
        Self::new_with_raw_value(model).ok()
    }

    #[must_use]
    pub const fn is_atom(self) -> bool {
        matches!(
            self,
            Self::AtomSilvermont
                | Self::AtomSilvermontMid
                | Self::AtomAirmont
                | Self::AtomSilvermontMid2
                | Self::AtomGoldmont
                | Self::AtomGoldmontD
                | Self::AtomGoldmontPlus
                | Self::AtomTremontD
                | Self::AtomTremont
                | Self::AtomTremontL
                | Self::AtomGracemont
        )
    }

    /// Whether `MSR_PLATFORM_INFO` and `MSR_TURBO_RATIO_LIMIT` hold the Core-line ratios: every
    /// model here but the Silvermont-generation Atoms and Xeon Phi.
    #[must_use]
    pub const fn has_core_ratios(self) -> bool {
        !matches!(
            self,
            Self::AtomSilvermont
                | Self::AtomSilvermontMid
                | Self::AtomAirmont
                | Self::AtomSilvermontMid2
                | Self::XeonPhiKnl
                | Self::XeonPhiKnm
        )
    }

    /// How this model's RAPL registers scale.
    #[must_use]
    pub const fn rapl(self) -> Flavor {
        match self {
            Self::HaswellX
            | Self::BroadwellX
            | Self::SkylakeX
            | Self::IceLakeX
            | Self::IceLakeD
            | Self::XeonPhiKnl
            | Self::XeonPhiKnm => Flavor::HaswellServer,
            Self::SapphireRapidsX | Self::EmeraldRapidsX => Flavor::SapphireRapidsServer,
            Self::AtomSilvermont | Self::AtomSilvermontMid | Self::AtomAirmont | Self::AtomSilvermontMid2 => {
                Flavor::Silvermont
            },
            _ => Flavor::Core,
        }
    }

    /// Which limit-reasons register this model has and how its bits read.
    #[must_use]
    pub const fn limit_reasons(self) -> Option<Layout> {
        match self {
            Self::Haswell
            | Self::HaswellX
            | Self::HaswellL
            | Self::HaswellG
            | Self::Broadwell
            | Self::BroadwellG
            | Self::BroadwellX
            | Self::BroadwellD => Some(Layout::Haswell),
            Self::SkylakeL
            | Self::Skylake
            | Self::KabyLakeL
            | Self::KabyLake
            | Self::CometLake
            | Self::CometLakeL
            | Self::CannonLakeL
            | Self::IceLake
            | Self::IceLakeL
            | Self::RocketLake
            | Self::TigerLakeL
            | Self::TigerLake
            | Self::AlderLake
            | Self::AlderLakeL
            | Self::RaptorLake
            | Self::RaptorLakeP
            | Self::RaptorLakeS => Some(Layout::Skylake),
            _ => None,
        }
    }

    /// The frequency of one HWP performance unit on this model's P-cores, in kHz, where it is not
    /// the 100 MHz bus ratio. Only hybrid parts scale differently.
    #[must_use]
    pub const fn p_core_khz_per_unit(self) -> Option<u32> {
        match self {
            Self::AlderLake
            | Self::AlderLakeL
            | Self::RaptorLake
            | Self::RaptorLakeP
            | Self::RaptorLakeS
            | Self::BartlettLake => Some(78_741),
            Self::MeteorLakeL => Some(80_000),
            Self::LunarLakeM => Some(86_957),
            _ => None,
        }
    }

    /// Whether this model may have `MSR_PPERF`; a read still has to succeed to count.
    #[must_use]
    pub const fn may_have_pperf(self) -> bool {
        !self.is_atom()
            && !matches!(
                self,
                Self::SandyBridge
                    | Self::SandyBridgeX
                    | Self::IvyBridge
                    | Self::IvyBridgeX
                    | Self::Haswell
                    | Self::HaswellX
                    | Self::HaswellL
                    | Self::HaswellG
                    | Self::Broadwell
                    | Self::BroadwellG
                    | Self::BroadwellX
                    | Self::BroadwellD
                    | Self::XeonPhiKnl
                    | Self::XeonPhiKnm
            )
    }
}
