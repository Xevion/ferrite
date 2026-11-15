//! `ferrite_mem::crew` jobs on the firmware's APs: what a core reports about itself, and the AP
//! task that runs a job under MP Services.

use ferrite_hal::Clock;
use ferrite_mem::crew::{Job, Platform, WideGuard};
use ferrite_mem::{Caps, Tier};

use crate::cpu::{FpuControl, VectorState, VectorSupport};
use crate::mp::{ApTask, StackUse};

/// The tiers that run on the calling core as its CR4 and XCR0 are now.
#[must_use]
pub fn usable_caps() -> Caps {
    let support = VectorSupport::probe();
    // SAFETY: the probe reads CPUID and XCR0 as they are now, and the `_usable` checks need both.
    unsafe { Caps::new(support.avx2_usable(), support.avx512f_usable()) }
}

/// Whether `tier` runs on the calling core as its CR4 and XCR0 are now.
#[must_use]
pub fn tier_usable(tier: Tier) -> bool {
    usable_caps().has(tier)
}

impl WideGuard for VectorState {
    fn wide(&self, f: &mut dyn FnMut()) {
        Self::wide(self, f);
    }
}

/// The calling core, asked through its control registers and CPUID.
#[derive(Clone, Copy, Debug)]
pub struct ThisCore;

impl Platform for ThisCore {
    type Wide = VectorState;

    fn sse_ready(&self) -> bool {
        FpuControl::read().sse_ready()
    }

    fn enable_wide(&self) -> Option<VectorState> {
        VectorState::enable().ok()
    }

    fn tier_usable(&self, tier: Tier) -> bool {
        tier_usable(tier)
    }
}

// SAFETY: the AP path runs ferrite-mem kernels on its own share, reads CPUID, CR4, XCR0 and the TSC,
// writes CR4 and XCR0 through VectorState (restored before return) and stores to atomics. No EFI
// call, allocation, logging or lock; the tests' state lives in the job, so the frames stay small.
unsafe impl<C: Clock + Sync> ApTask for Job<'_, C> {
    fn run(&self) {
        self.run_ap(&ThisCore);
    }

    fn finish(&self, stack: StackUse) {
        self.record_stack(stack.bytes());
    }
}
