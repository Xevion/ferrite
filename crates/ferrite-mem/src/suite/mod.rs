//! The memory tests, each a steppable state machine, and the suite that runs them pass after pass.
//!
//! The memory under test is a set of regions ([`Regions`]), read as one run of words: a test plans
//! over the whole run, so its sequences continue from one region into the next, and only the tests
//! that need contiguous memory (the address walk, a block-move window) work region by region.
//!
//! A test is a fixed list of phases (a fill, a verify, an inversion, a block move over a window,
//! a wait), each a walk over the region or part of it. [`Test::step`] does one bounded chunk of one
//! phase and never crosses a phase boundary, so the caller can interleave its own work between
//! steps, and can run steps of the AVX tiers with interrupts off in short chunks. Each phase picks
//! its kernel from [`Kernels`]: pure stores the fill kernel, read-check-write steps the
//! read-modify-write kernel, reads the verify kernel.
//!
//! Most tests are memtest86+'s, the rest come from ferrite's pattern set. Rowhammer is deferred: picking
//! aggressor rows next to a victim needs the DRAM address mapping (channel, rank, bank, row), which
//! nothing here knows yet.

use alloc::vec::Vec;
use core::fmt;
use core::time::Duration;

use ferrite_hal::Instant;

use crate::caps::{Caps, Kernel, Stores};
use crate::fault::Faults;
use crate::region::{Layout, Regions};

mod plan;
mod run;
mod shape;
mod share;
#[cfg(test)]
mod tests;

pub use plan::Span;
pub use run::{Progress, Test};
pub use shape::{Place, Shape, Stage};
pub use share::{Mode, shares};

/// What a run covers.
///
/// The first four are tiers, cumulative: each runs every test of its tier and the tiers below it,
/// with [`Config::DEFAULT`]. [`Preset::Memtest86Plus`] is memtest86+'s own tests with its default
/// counts, so a run can be compared with memtest86+ without editing constants.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, strum::IntoStaticStr, strum::VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum Preset {
    /// A fast sanity check.
    Quick,
    /// The default run.
    Standard,
    /// Every test, including the slow and the opt-in ones (bit fade).
    Thorough,
    /// Tests that may induce faults as a side effect; none exist yet (rowhammer is deferred).
    Destructive,
    /// The tests memtest86+ v9.00 runs, with its default counts: 128 random rounds, 8 modulo rounds
    /// and bit fade as 4 waits of 120 s.
    Memtest86Plus,
}

impl Preset {
    pub const ALL: &'static [Self] = <Self as strum::VariantArray>::VARIANTS;

    /// Whether a run of this preset includes `kind`.
    #[must_use]
    pub fn runs(self, kind: TestKind) -> bool {
        match self {
            Self::Memtest86Plus => kind.in_memtest86_plus(),
            tier => kind.preset() <= tier,
        }
    }

    /// The repetition counts of a run of this preset.
    #[must_use]
    pub const fn config(self) -> Config {
        match self {
            Self::Memtest86Plus => Config::MEMTEST86_PLUS,
            Self::Quick | Self::Standard | Self::Thorough | Self::Destructive => Config::DEFAULT,
        }
    }
}

/// A class of fault a test is designed to find.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, strum::IntoStaticStr)]
#[strum(serialize_all = "kebab-case")]
pub enum FaultClass {
    /// A cell stuck at 0 or 1 whatever is written.
    StuckAt,
    /// A cell that fails to make a commanded 0 to 1 or 1 to 0 transition.
    Transition,
    /// A write to, or a transition in, one cell disturbs another.
    Coupling,
    /// The address decoder maps two addresses to one cell, or a cell to no address.
    AddressDecoder,
    /// A cell loses its charge before it is refreshed.
    Retention,
    /// Activations of an aggressor row flip bits in a neighbouring row; no test targets it yet.
    RowDisturbance,
    /// The memory interface: signal integrity at read/write turnarounds, marginal training, supply droop.
    Interface,
}

/// Every test, in the order a pass runs them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, strum::VariantArray)]
pub enum TestKind {
    /// memtest86+ test 0. memtest86+ runs it with the cache off; here every access is followed by a
    /// `clflush`, so each read still comes from DRAM.
    AddressWalk,
    /// memtest86+ test 2: own address over the whole region, then checked, and again complemented.
    OwnAddress,
    SolidBits,
    Checkerboard,
    MarchCMinus,
    /// memtest86+ test 3.
    BusStress,
    /// memtest86+ test 4.
    MovingInversions,
    /// memtest86+ test 5; every fourth round is one random word everywhere rather than a sequence.
    MovingInversionsRandom,
    /// memtest86+ test 6.
    MovingInversions8Bit,
    WalkingOnes,
    WalkingZeros,
    /// memtest86+ test 7.
    Modulo20,
    /// memtest86+ test 8.
    BlockMove,
    /// memtest86+ test 9.
    MovingInversions64Bit,
    /// memtest86+ test 1: own address, filled and checked one window at a time.
    OwnAddressWindow,
    /// memtest86+ test 10.
    BitFade,
}

impl TestKind {
    pub const ALL: &'static [Self] = <Self as strum::VariantArray>::VARIANTS;

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::AddressWalk => "address, walking ones",
            Self::OwnAddress => "address, own address",
            Self::SolidBits => "solid bits",
            Self::Checkerboard => "checkerboard",
            Self::MarchCMinus => "march c-",
            Self::BusStress => "bus stress, r/w turnaround",
            Self::MovingInversions => "moving inversions, 1s and 0s",
            Self::MovingInversionsRandom => "moving inversions, random",
            Self::MovingInversions8Bit => "moving inversions, 8-bit",
            Self::WalkingOnes => "walking ones",
            Self::WalkingZeros => "walking zeros",
            Self::Modulo20 => "modulo 20, random",
            Self::BlockMove => "block move",
            Self::MovingInversions64Bit => "moving inversions, 64-bit shifting",
            Self::OwnAddressWindow => "address, own address in window",
            Self::BitFade => "bit fade",
        }
    }

    #[must_use]
    pub const fn fault_classes(self) -> &'static [FaultClass] {
        use FaultClass::{AddressDecoder, Coupling, Interface, Retention, StuckAt, Transition};
        match self {
            Self::AddressWalk | Self::OwnAddress | Self::OwnAddressWindow => &[AddressDecoder],
            Self::SolidBits => &[StuckAt],
            Self::Checkerboard | Self::WalkingOnes | Self::WalkingZeros | Self::BlockMove => &[StuckAt, Coupling],
            Self::MarchCMinus => &[StuckAt, Transition, AddressDecoder, Coupling],
            Self::BusStress => &[Interface],
            Self::MovingInversions
            | Self::MovingInversionsRandom
            | Self::MovingInversions8Bit
            | Self::MovingInversions64Bit => &[StuckAt, Transition, Coupling],
            Self::Modulo20 => &[StuckAt, Coupling, AddressDecoder],
            Self::BitFade => &[Retention],
        }
    }

    /// The lowest preset that runs the test.
    #[must_use]
    pub const fn preset(self) -> Preset {
        match self {
            Self::AddressWalk | Self::OwnAddress | Self::SolidBits | Self::Checkerboard => Preset::Quick,
            Self::OwnAddressWindow | Self::BitFade => Preset::Thorough,
            _ => Preset::Standard,
        }
    }

    /// Whether memtest86+ has the test; the others are ferrite's own.
    #[must_use]
    pub const fn in_memtest86_plus(self) -> bool {
        !matches!(
            self,
            Self::SolidBits | Self::Checkerboard | Self::MarchCMinus | Self::WalkingOnes | Self::WalkingZeros
        )
    }

    /// The tests `preset` runs, in order.
    pub fn sequence(preset: Preset) -> impl Iterator<Item = Self> + Clone {
        Self::ALL.iter().copied().filter(move |&k| preset.runs(k))
    }
}

impl fmt::Display for TestKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Repetition counts and sizes. Where memtest86+ has a count it is named; some defaults are lower.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// Mixed with the test and the pass number into every random pattern, so a pass is reproducible.
    pub seed: u64,
    /// Up-and-down inversion pairs per pattern for 1s and 0s (memtest86+: 6).
    pub fixed_iterations: u32,
    /// Inversion pairs per pattern for the 8-bit patterns (memtest86+: 3).
    pub byte_iterations: u32,
    /// Fill and two inversions per round (memtest86+: 128).
    pub random_rounds: u32,
    /// Inversion pairs per bit offset for the 64-bit shifting pattern (memtest86+: 1).
    pub shifting_iterations: u32,
    /// Rounds of the 20 offsets (memtest86+: 8).
    pub modulo_rounds: u32,
    /// Moves per window (memtest86+: 81).
    pub move_iterations: u32,
    /// Fill and verify sweeps (memtest86+: 32).
    pub bus_iterations: u32,
    /// How long each bit-fade round leaves memory untouched.
    pub fade: Duration,
    pub address_window_bytes: usize,
    pub move_window_bytes: usize,
}

impl Config {
    pub const DEFAULT: Self = Self {
        seed: 0x6665_7272_6974_6521,
        fixed_iterations: 6,
        byte_iterations: 3,
        random_rounds: 32,
        shifting_iterations: 1,
        modulo_rounds: 1,
        move_iterations: 81,
        bus_iterations: 32,
        fade: Duration::from_secs(120),
        address_window_bytes: 2 << 20,
        move_window_bytes: 1 << 20,
    };

    /// memtest86+ v9.00's counts: every count at its default there.
    pub const MEMTEST86_PLUS: Self = Self { random_rounds: 128, modulo_rounds: 8, ..Self::DEFAULT };
}

impl Default for Config {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The kernel for each kind of phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Kernels {
    /// Pure stores: fills and copies.
    pub fill: Kernel,
    /// Pure loads.
    pub verify: Kernel,
    /// Read, check and rewrite.
    pub rmw: Kernel,
}

impl Kernels {
    /// Non-temporal stores of the widest tier for fills (they skip the read-for-ownership), its
    /// temporal kernel for the rest (an inversion rewrites lines it has just read, which non-temporal
    /// stores would evict).
    #[must_use]
    pub fn for_caps(caps: Caps) -> Self {
        let best = caps.best();
        let temporal = caps.kernel(best, Stores::Temporal).unwrap_or(Kernel::SCALAR);
        Self { fill: caps.kernel(best, Stores::NonTemporal).unwrap_or(temporal), verify: temporal, rmw: temporal }
    }

    /// One kernel for everything.
    #[must_use]
    pub const fn uniform(kernel: Kernel) -> Self {
        Self { fill: kernel, verify: kernel, rmw: kernel }
    }

    /// Whether any of them uses YMM or ZMM registers.
    #[must_use]
    pub const fn is_wide(&self) -> bool {
        self.fill.tier().is_wide() || self.verify.tier().is_wide() || self.rmw.tier().is_wide()
    }
}

impl fmt::Display for Kernels {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "fill {}, verify {}, rmw {}", self.fill, self.verify, self.rmw)
    }
}

/// Where a suite is in its preset: which test, which pass, how much of the pass is done. Apart from
/// the memory, so a test run on several cores can be counted done while the memory is lent out.
#[derive(Debug)]
pub struct Schedule {
    layout: Layout,
    kernels: Kernels,
    config: Config,
    preset: Preset,
    test: Test,
    passes: u64,
    /// Work of the tests already done in this pass, and of the whole pass.
    pass_done: u64,
    pass_total: u64,
    /// The shape of each test of the preset, in order, over the whole layout.
    shapes: Vec<Shape>,
}

impl Schedule {
    #[must_use]
    pub fn new(layout: Layout, kernels: Kernels, config: Config, preset: Preset) -> Self {
        let first = TestKind::sequence(preset).next().unwrap_or(TestKind::AddressWalk);
        let shapes: Vec<Shape> = TestKind::sequence(preset).map(|k| Shape::of(k, &config, 0, &layout)).collect();
        let pass_total = shapes.iter().map(Shape::work).sum();
        let test = Test::for_share(first, config, 0, &layout, layout.all());
        Self { layout, kernels, config, preset, test, passes: 0, pass_done: 0, pass_total, shapes }
    }

    /// A schedule whose first pass is `pass`, so a rebuilt one carries on with the patterns that follow.
    #[must_use]
    pub fn starting_at(layout: Layout, kernels: Kernels, config: Config, preset: Preset, pass: u64) -> Self {
        let mut schedule = Self::new(layout, kernels, config, preset);
        schedule.passes = pass;
        schedule.test = Test::for_share(schedule.test.kind(), config, pass, &schedule.layout, schedule.layout.all());
        schedule
    }

    /// Count the current test done and move on, when it was run elsewhere: split into shares over
    /// several cores, each with its own [`Test::for_share`].
    pub fn finish_test(&mut self) {
        self.next_test();
    }

    fn next_test(&mut self) {
        let kind = self.test.kind();
        self.pass_done += self.test.total();
        let next = if let Some(next) = TestKind::sequence(self.preset).skip_while(|&k| k != kind).nth(1) {
            next
        } else {
            self.passes += 1;
            self.pass_done = 0;
            TestKind::sequence(self.preset).next().unwrap_or(kind)
        };
        self.test = Test::for_share(next, self.config, self.passes, &self.layout, self.layout.all());
    }

    #[must_use]
    pub const fn test(&self) -> &Test {
        &self.test
    }

    /// Passes completed.
    #[must_use]
    pub const fn passes(&self) -> u64 {
        self.passes
    }

    /// The current test's 1-based position in the preset, and how many tests it has.
    #[must_use]
    pub fn position(&self) -> (usize, usize) {
        let kind = self.test.kind();
        let index = TestKind::sequence(self.preset).position(|k| k == kind).unwrap_or(0);
        (index + 1, TestKind::sequence(self.preset).count())
    }

    /// Progress through the current pass, 0 to 100.
    #[must_use]
    pub fn percent(&self) -> u16 {
        percent(self.pass_done + self.test.done(), self.pass_total)
    }

    /// The shape of each test of the preset, in the order a pass runs them.
    #[must_use]
    pub fn shapes(&self) -> &[Shape] {
        &self.shapes
    }

    /// The current test's 0-based index among [`Schedule::shapes`].
    #[must_use]
    pub fn index(&self) -> usize {
        self.position().0.saturating_sub(1)
    }

    /// Work of the pass's tests before the current one, and of the whole pass, for progress of a
    /// test run elsewhere.
    #[must_use]
    pub const fn pass_work(&self) -> (u64, u64) {
        (self.pass_done, self.pass_total)
    }

    /// The regions every test is planned over.
    #[must_use]
    pub const fn layout(&self) -> &Layout {
        &self.layout
    }

    #[must_use]
    pub const fn config(&self) -> Config {
        self.config
    }

    #[must_use]
    pub const fn kernels(&self) -> Kernels {
        self.kernels
    }

    #[must_use]
    pub const fn preset(&self) -> Preset {
        self.preset
    }
}

/// The tests of a preset run in order over a set of regions, pass after pass.
#[derive(Debug)]
pub struct Suite<'a> {
    regions: Regions<'a>,
    schedule: Schedule,
}

impl<'a> Suite<'a> {
    #[must_use]
    pub fn new(regions: Regions<'a>, kernels: Kernels, config: Config, preset: Preset) -> Self {
        let schedule = Schedule::new(regions.layout(), kernels, config, preset);
        Self { regions, schedule }
    }

    /// A suite whose first pass is `pass`.
    #[must_use]
    pub fn starting_at(regions: Regions<'a>, kernels: Kernels, config: Config, preset: Preset, pass: u64) -> Self {
        let schedule = Schedule::starting_at(regions.layout(), kernels, config, preset, pass);
        Self { regions, schedule }
    }

    /// Run one bounded chunk of the current test, moving on to the next test (or pass) at its end.
    pub fn step(&mut self, budget_bytes: usize, now: Instant, faults: &mut Faults<'_>) -> Progress {
        let schedule = &mut self.schedule;
        match schedule.test.step(&mut self.regions, &schedule.kernels, budget_bytes, now, faults) {
            Progress::Done => {
                schedule.next_test();
                Progress::Running
            },
            progress => progress,
        }
    }

    /// Count the current test done and move on, when it was run elsewhere.
    pub fn finish_test(&mut self) {
        self.schedule.finish_test();
    }

    /// The memory, to split the current test over several cores, and the schedule, which stays
    /// usable while the memory is lent out. The suite's own [`Test`] keeps no state in the memory,
    /// so such a run must start from the test's first phase.
    pub const fn split(&mut self) -> (&mut Regions<'a>, &mut Schedule) {
        (&mut self.regions, &mut self.schedule)
    }

    #[must_use]
    pub const fn schedule(&self) -> &Schedule {
        &self.schedule
    }

    #[must_use]
    pub const fn test(&self) -> &Test {
        self.schedule.test()
    }

    /// Passes completed.
    #[must_use]
    pub const fn passes(&self) -> u64 {
        self.schedule.passes
    }

    /// The current test's 1-based position in the preset, and how many tests it has.
    #[must_use]
    pub fn position(&self) -> (usize, usize) {
        self.schedule.position()
    }

    /// Progress through the current pass, 0 to 100.
    #[must_use]
    pub fn percent(&self) -> u16 {
        self.schedule.percent()
    }

    #[must_use]
    pub const fn regions(&self) -> &Regions<'a> {
        &self.regions
    }

    #[must_use]
    pub const fn config(&self) -> Config {
        self.schedule.config
    }

    #[must_use]
    pub const fn kernels(&self) -> Kernels {
        self.schedule.kernels
    }

    #[must_use]
    pub const fn preset(&self) -> Preset {
        self.schedule.preset
    }
}

/// `done` of `total`, 0 to 100.
#[must_use]
pub fn percent(done: u64, total: u64) -> u16 {
    if total == 0 { 0 } else { (u128::from(done.min(total)) * 100 / u128::from(total)) as u16 }
}
