//! Test support: a sample status, and the allocation count the tests here use.

use ferrite_mem::analysis::Classification;
use ferrite_mem::suite::{Mode, Preset};

use crate::sweep::SweepStatus;
pub use crate::testing::count;

/// A status for a sweep that has just started.
pub fn idle_status() -> SweepStatus {
    SweepStatus {
        bytes: 1 << 20,
        test: "own-address",
        position: (1, 9),
        test_percent: 0,
        percent: 0,
        passes: 0,
        tier: "sse2",
        mode: Mode::Single,
        cpus: 1,
        faults: 0,
        first_fault: None,
        analysis: Classification::Clean,
        preset: Preset::Standard,
        paused: false,
    }
}
