//! Which tiers the CPU can run, and the kernels built from them.

use core::fmt;

/// An instruction-set tier, from plain scalar code up.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, strum::IntoStaticStr, strum::VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum Tier {
    /// Volatile `u64` loads and stores.
    Scalar,
    /// `rep stosq` for constant fills and `rep movsq` for copies; every other block is [`Tier::Scalar`].
    RepString,
    /// 128-bit SSE2, the x86-64 baseline.
    Sse2,
    /// 256-bit AVX2: needs AVX state enabled in XCR0.
    Avx2,
    /// 512-bit AVX-512F: needs the opmask and ZMM state enabled in XCR0.
    Avx512,
}

impl Tier {
    /// Whether the tier has non-temporal stores.
    #[must_use]
    pub const fn has_non_temporal(self) -> bool {
        matches!(self, Self::Sse2 | Self::Avx2 | Self::Avx512)
    }

    /// Whether the tier uses YMM or ZMM registers, whose upper halves firmware interrupts do not save.
    #[must_use]
    pub const fn is_wide(self) -> bool {
        matches!(self, Self::Avx2 | Self::Avx512)
    }
}

/// How a kernel stores: through the caches, or around them with an `sfence` at the end of the chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, strum::IntoStaticStr)]
#[strum(serialize_all = "kebab-case")]
pub enum Stores {
    Temporal,
    NonTemporal,
}

/// A tier and store kind the CPU can run. Only [`Caps`] makes one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Kernel {
    tier: Tier,
    stores: Stores,
}

impl Kernel {
    /// Scalar temporal stores, which every x86-64 CPU runs.
    pub const SCALAR: Self = Self { tier: Tier::Scalar, stores: Stores::Temporal };

    #[must_use]
    pub const fn tier(self) -> Tier {
        self.tier
    }

    #[must_use]
    pub const fn stores(self) -> Stores {
        self.stores
    }
}

impl fmt::Display for Kernel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tier: &str = self.tier.into();
        match self.stores {
            Stores::Temporal => f.write_str(tier),
            Stores::NonTemporal => write!(f, "{tier}-nt"),
        }
    }
}

/// The tiers usable on this CPU right now. Scalar, `rep` strings and SSE2 are always present.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    avx2: bool,
    avx512: bool,
}

impl Caps {
    /// The x86-64 baseline: scalar, `rep` strings and SSE2.
    pub const BASELINE: Self = Self { avx2: false, avx512: false };

    /// # Safety
    /// `avx2` must be true only if AVX2 instructions run now (CPUID AVX2, CR4.OSXSAVE, XCR0 SSE and
    /// AVX state); `avx512f` only if AVX-512F runs (also XCR0 opmask, `ZMM_Hi256` and `Hi16_ZMM`).
    #[must_use]
    pub const unsafe fn new(avx2: bool, avx512f: bool) -> Self {
        Self { avx2, avx512: avx512f }
    }

    #[must_use]
    pub const fn has(self, tier: Tier) -> bool {
        match tier {
            Tier::Scalar | Tier::RepString | Tier::Sse2 => true,
            Tier::Avx2 => self.avx2,
            Tier::Avx512 => self.avx512,
        }
    }

    /// The kernel for `tier` and `stores`, if the CPU runs the tier and the tier has that store kind.
    #[must_use]
    pub const fn kernel(self, tier: Tier, stores: Stores) -> Option<Kernel> {
        if !self.has(tier) || (matches!(stores, Stores::NonTemporal) && !tier.has_non_temporal()) {
            return None;
        }
        Some(Kernel { tier, stores })
    }

    /// Usable tiers, lowest first.
    pub fn tiers(self) -> impl Iterator<Item = Tier> {
        <Tier as strum::VariantArray>::VARIANTS.iter().copied().filter(move |&t| self.has(t))
    }

    /// Every usable kernel, lowest tier first, temporal before non-temporal.
    pub fn kernels(self) -> impl Iterator<Item = Kernel> {
        self.tiers().flat_map(move |t| {
            [Stores::Temporal, Stores::NonTemporal].into_iter().filter_map(move |s| self.kernel(t, s))
        })
    }

    /// The widest usable tier.
    #[must_use]
    pub fn best(self) -> Tier {
        self.tiers().last().unwrap_or(Tier::Scalar)
    }
}
