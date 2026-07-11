//! Static metadata describing what each [`Pattern`](super::Pattern) detects and
//! how expensive it is to run.

/// A class of DRAM fault a pattern is designed to detect.
///
/// The four academic coupling sub-types collapse into a single
/// [`Coupling`](Self::Coupling) variant, since a field tester acts on all of
/// them the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
pub enum FaultClass {
    /// Cell permanently stuck at 0 or 1 regardless of writes.
    StuckAt,
    /// Cell fails to make a commanded 0->1 or 1->0 transition.
    Transition,
    /// A transition or state in one cell disturbs an adjacent cell.
    Coupling,
    /// Address decoder maps multiple addresses to one cell, or a cell to no
    /// address.
    AddressDecoder,
    /// Cell loses its charge before the controller refreshes it.
    Retention,
    /// Rapid activation of an aggressor row flips bits in a victim row.
    RowDisturbance,
}

impl FaultClass {
    /// Human-readable name for display.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::StuckAt => "Stuck-At",
            Self::Transition => "Transition",
            Self::Coupling => "Coupling",
            Self::AddressDecoder => "Address Decoder",
            Self::Retention => "Retention",
            Self::RowDisturbance => "Row Disturbance",
        }
    }
}

/// Time complexity of a pattern relative to buffer size `n`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Complexity {
    /// O(n): one pass, or a fixed small number of passes.
    Linear,
    /// O(k*n): `k` passes over the whole buffer (e.g. walking bits at k=64,
    /// March C- at k=10).
    LinearK(u8),
    /// O(n^2): exhaustive coupling tests.
    Quadratic,
}

/// Preset tier a pattern participates in.
///
/// Tiers are cumulative in coverage: [`Thorough`](Self::Thorough) is a superset
/// of [`Standard`](Self::Standard), which is a superset of [`Quick`](Self::Quick).
/// [`Destructive`](Self::Destructive) is orthogonal -- opt-in patterns that may
/// induce errors as a side effect and are never run implicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
pub enum PatternTier {
    /// Fast sanity check after boot or in CI.
    Quick,
    /// Default diagnostic run.
    Standard,
    /// Full trusted suite.
    Thorough,
    /// Opt-in patterns that may deliberately induce errors.
    Destructive,
}

/// Static description of a pattern's fault coverage and cost.
///
/// Obtained via [`Pattern::metadata`](super::Pattern::metadata).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct PatternMetadata {
    /// Fault classes this pattern is designed to detect.
    pub fault_classes: &'static [FaultClass],
    /// Time complexity relative to buffer size.
    pub complexity: Complexity,
    /// Whether physical address ordering is required for full effectiveness.
    ///
    /// `false` here means the pattern's headline coverage holds regardless of
    /// page layout; coupling coverage may still improve when physical neighbors
    /// happen to be adjacent in the buffer.
    pub requires_physical_order: bool,
    /// Whether this pattern may induce errors as a side effect (rowhammer).
    pub is_destructive: bool,
    /// Preset tiers that include this pattern.
    pub tiers: &'static [PatternTier],
}
