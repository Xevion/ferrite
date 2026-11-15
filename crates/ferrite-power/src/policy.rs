//! What the app asks of the processor's clock, as the settings file says it.

use core::fmt;

/// How the clock is managed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, strum::IntoStaticStr, strum::VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum ClockMode {
    /// Leave the clock as the firmware set it.
    Firmware,
    /// The highest level every core allows, all the time.
    Max,
    /// The lowest ceiling per test that keeps it within the tolerance of its fastest.
    #[default]
    Auto,
    /// Run each test at each step of clock ceilings and record its speed and power.
    Calibrate,
}

/// How much slower than its fastest a test may run in [`ClockMode::Auto`], in whole percent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Tolerance(u8);

impl Tolerance {
    pub const MAX: u8 = 50;
    pub const DEFAULT: Self = Self(3);

    /// `None` above [`Tolerance::MAX`] percent.
    #[must_use]
    pub const fn new(percent: u8) -> Option<Self> {
        if percent <= Self::MAX { Some(Self(percent)) } else { None }
    }

    #[must_use]
    pub const fn percent(self) -> u8 {
        self.0
    }
}

impl Default for Tolerance {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A temperature ceiling in whole degrees Celsius.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TempLimit(i16);

impl TempLimit {
    pub const RANGE: core::ops::RangeInclusive<i16> = 40..=105;

    /// `None` outside [`TempLimit::RANGE`].
    #[must_use]
    pub fn new(celsius: i16) -> Option<Self> {
        Self::RANGE.contains(&celsius).then_some(Self(celsius))
    }

    #[must_use]
    pub const fn celsius(self) -> i16 {
        self.0
    }
}

/// The `[cpu]` settings once checked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Policy {
    pub clock: ClockMode,
    /// Lower the ceiling to stay under this; `None` leaves heat to the processor's own protection.
    pub max_temp: Option<TempLimit>,
    pub tolerance: Tolerance,
}

impl fmt::Display for Policy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "clock {}", <&str>::from(self.clock))?;
        if let Some(limit) = self.max_temp {
            write!(f, ", under {} C", limit.celsius())?;
        }
        write!(f, ", tolerance {}%", self.tolerance.percent())
    }
}
