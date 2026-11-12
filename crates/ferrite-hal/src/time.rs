//! Monotonic time points and DMA page counts.

use core::ops::{Add, Sub};
use core::time::Duration;

use crate::PAGE_SIZE;

/// A point on a monotonic clock, as time since that clock's own epoch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Instant(Duration);

impl Instant {
    #[must_use]
    pub const fn from_epoch(since_epoch: Duration) -> Self {
        Self(since_epoch)
    }

    /// Time since the clock's epoch (for the UEFI clock, since it was started).
    #[must_use]
    pub const fn since_epoch(self) -> Duration {
        self.0
    }

    #[must_use]
    pub const fn checked_add(self, d: Duration) -> Option<Self> {
        match self.0.checked_add(d) {
            Some(t) => Some(Self(t)),
            None => None,
        }
    }

    /// Time from `earlier` to `self`, zero if `earlier` is later.
    #[must_use]
    pub const fn saturating_duration_since(self, earlier: Self) -> Duration {
        self.0.saturating_sub(earlier.0)
    }
}

/// Saturates at zero, like `std::time::Instant`.
impl Sub for Instant {
    type Output = Duration;

    fn sub(self, earlier: Self) -> Duration {
        self.saturating_duration_since(earlier)
    }
}

/// # Panics
/// On overflow, which needs a deadline some 584 billion years out.
impl Add<Duration> for Instant {
    type Output = Self;

    fn add(self, d: Duration) -> Self {
        self.checked_add(d).unwrap_or_else(|| panic!("instant overflow"))
    }
}

/// A number of DMA pages.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PageCount(usize);

impl PageCount {
    #[must_use]
    pub const fn new(pages: usize) -> Self {
        Self(pages)
    }

    #[must_use]
    pub const fn get(self) -> usize {
        self.0
    }

    /// Size in bytes.
    ///
    /// # Panics
    /// If the byte count overflows `usize`.
    #[must_use]
    pub const fn bytes(self) -> usize {
        match self.0.checked_mul(PAGE_SIZE) {
            Some(b) => b,
            None => panic!("page count overflows usize bytes"),
        }
    }

    /// Pages needed to hold `bytes`.
    #[must_use]
    pub const fn for_bytes(bytes: usize) -> Self {
        Self(bytes.div_ceil(PAGE_SIZE))
    }

    #[must_use]
    pub const fn checked_add(self, other: Self) -> Option<Self> {
        match self.0.checked_add(other.0) {
            Some(p) => Some(Self(p)),
            None => None,
        }
    }
}
