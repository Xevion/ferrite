//! The scheduler's platform in a Linux process: the std clock, no interrupts, and mmap'd task stacks
//! with a no-access guard page below each.

use std::num::NonZeroUsize;
use std::time::Instant as StdInstant;

use corosensei::stack::{Stack, StackPointer};
use ferrite_hal::Instant;
use ferrite_lkpi::sched::{Platform, StackAllocError, StackSource};

const PAGE: usize = 4096;

pub struct HostPlatform {
    epoch: StdInstant,
}

impl HostPlatform {
    pub fn new() -> Self {
        Self { epoch: StdInstant::now() }
    }
}

impl Platform for HostPlatform {
    fn now(&self) -> Instant {
        Instant::from_epoch(self.epoch.elapsed())
    }

    /// The main loop polls again at once while tasks are runnable, and there are no timers yet.
    fn arm_wakeup(&mut self, _at: Option<Instant>) {}

    fn take_pending_irqs(&mut self) -> bool {
        false
    }
}

/// A mapping of a guard page and `size` bytes of stack above it.
pub struct GuardedStack {
    start: usize,
    size: usize,
}

// SAFETY: the range above the guard page is mapped read-write, page aligned, and owned by this value
// until `release` unmaps it.
unsafe impl Stack for GuardedStack {
    fn base(&self) -> StackPointer {
        NonZeroUsize::new(self.start + PAGE + self.size).unwrap_or(NonZeroUsize::MIN)
    }

    fn limit(&self) -> StackPointer {
        NonZeroUsize::new(self.start + PAGE).unwrap_or(NonZeroUsize::MIN)
    }
}

pub struct MmapStacks;

impl StackSource for MmapStacks {
    type Stack = GuardedStack;

    fn allocate(&mut self, size: usize) -> Result<GuardedStack, StackAllocError> {
        let size = size.next_multiple_of(PAGE);
        let length = size + PAGE;
        // SAFETY: a fresh anonymous mapping that nothing else refers to.
        let start = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if start == libc::MAP_FAILED {
            log::error!("mmap of a {length}-byte stack failed: {}", std::io::Error::last_os_error());
            return Err(StackAllocError { size });
        }
        // SAFETY: the first page of the mapping just made.
        if unsafe { libc::mprotect(start, PAGE, libc::PROT_NONE) } != 0 {
            log::error!("mprotect of a stack guard page failed: {}", std::io::Error::last_os_error());
            // SAFETY: the mapping just made, which nothing uses.
            unsafe { libc::munmap(start, length) };
            return Err(StackAllocError { size });
        }
        Ok(GuardedStack { start: start as usize, size })
    }

    fn release(&mut self, stack: GuardedStack) {
        let length = stack.size + PAGE;
        // SAFETY: the mapping `allocate` made, which the scheduler no longer runs on.
        if unsafe { libc::munmap(std::ptr::with_exposed_provenance_mut(stack.start), length) } != 0 {
            log::warn!("munmap of a stack at {:#x} failed: {}", stack.start, std::io::Error::last_os_error());
        }
    }
}

/// Seed bytes from the host kernel's entropy pool.
pub fn entropy() -> Result<[u8; ferrite_lkpi::random::SEED_LEN], std::io::Error> {
    let mut seed = [0_u8; ferrite_lkpi::random::SEED_LEN];
    // SAFETY: the buffer is writable for its whole length.
    let got = unsafe { libc::getrandom(seed.as_mut_ptr().cast(), seed.len(), 0) };
    if usize::try_from(got).is_ok_and(|n| n == seed.len()) { Ok(seed) } else { Err(std::io::Error::last_os_error()) }
}
