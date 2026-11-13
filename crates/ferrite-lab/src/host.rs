//! The scheduler's platform in a Linux process: the std clock, no interrupts, mmap'd task stacks
//! with a no-access guard page below each, and an empty PCI device list.

use std::io::ErrorKind;
use std::num::NonZeroUsize;
use std::path::{Component, Path, PathBuf};
use std::time::Instant as StdInstant;

use corosensei::stack::{Stack, StackPointer};
use ferrite_hal::{BusAddr, Instant};
use ferrite_lkpi::firmware::{FirmwareError, FirmwareProvider};
use ferrite_lkpi::mem::{ARENA_ALIGN, Arena, ArenaError};
use ferrite_lkpi::pci::{PciDevice, PciError, PciPlatform, Width};
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

/// Where the lab's kernel arena sits in device address space: below 4 GiB for 32-bit DMA, and the IOVA
/// it is to be mapped at once VFIO maps it.
pub const ARENA_IOVA: u64 = 0x4000_0000;

#[derive(Debug)]
pub enum ArenaMapError {
    Mmap(std::io::Error),
    Arena(ArenaError),
}

impl std::fmt::Display for ArenaMapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mmap(error) => write!(f, "mmap of the kernel arena failed: {error}"),
            Self::Arena(error) => write!(f, "{error}"),
        }
    }
}

/// An anonymous mapping of `size` bytes, aligned like its IOVA, kept for the life of the process.
/// Untouched pages cost nothing (`MAP_NORESERVE`).
pub fn arena(size: usize) -> Result<Arena, ArenaMapError> {
    let length = size + ARENA_ALIGN;
    // SAFETY: a fresh anonymous mapping that nothing else refers to.
    let start = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            length,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    if start == libc::MAP_FAILED {
        return Err(ArenaMapError::Mmap(std::io::Error::last_os_error()));
    }
    let base = start.cast::<u8>().wrapping_add(start.addr().next_multiple_of(ARENA_ALIGN) - start.addr());
    let Some(base) = std::ptr::NonNull::new(base) else {
        return Err(ArenaMapError::Mmap(std::io::Error::from(ErrorKind::InvalidData)));
    };
    // SAFETY: `size` bytes from `base` lie inside the mapping, which is never unmapped or used elsewhere;
    // the lab has no device, so the bus address is only the one DMA mapping will use.
    unsafe { Arena::new(base, BusAddr::new(ARENA_IOVA), size) }.map_err(ArenaMapError::Arena)
}

/// Seed bytes from the host kernel's entropy pool.
pub fn entropy() -> Result<[u8; ferrite_lkpi::random::SEED_LEN], std::io::Error> {
    let mut seed = [0_u8; ferrite_lkpi::random::SEED_LEN];
    // SAFETY: the buffer is writable for its whole length.
    let got = unsafe { libc::getrandom(seed.as_mut_ptr().cast(), seed.len(), 0) };
    if usize::try_from(got).is_ok_and(|n| n == seed.len()) { Ok(seed) } else { Err(std::io::Error::last_os_error()) }
}

/// Firmware files read from a directory, `/lib/firmware` in the lab.
pub struct FirmwareDir {
    root: PathBuf,
}

impl FirmwareDir {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl FirmwareProvider for FirmwareDir {
    fn load(&self, name: &str) -> Result<Vec<u8>, FirmwareError> {
        let relative = Path::new(name);
        let confined = !name.is_empty() && relative.components().all(|part| matches!(part, Component::Normal(_)));
        if !confined {
            log::warn!("refusing firmware name {name:?}: not a path below the firmware directory");
            return Err(FirmwareError::NotFound { name: name.to_owned() });
        }
        match std::fs::read(self.root.join(relative)) {
            Ok(bytes) => {
                log::debug!("firmware {name}: {} bytes", bytes.len());
                Ok(bytes)
            },
            Err(error) if error.kind() == ErrorKind::NotFound => Err(FirmwareError::NotFound { name: name.to_owned() }),
            Err(error) => Err(FirmwareError::Unreadable { name: name.to_owned(), reason: error.to_string() }),
        }
    }
}

/// No PCI devices yet; the VFIO-assigned card will be the first.
pub struct NoPci;

impl PciPlatform for NoPci {
    fn devices(&self) -> &[PciDevice] {
        &[]
    }

    fn config_read(&self, index: usize, _: u16, _: Width) -> Result<u32, PciError> {
        Err(PciError::NoDevice { index })
    }

    fn config_write(&self, index: usize, _: u16, _: Width, _: u32) -> Result<(), PciError> {
        Err(PciError::NoDevice { index })
    }

    fn enable(&self, index: usize) -> Result<(), PciError> {
        Err(PciError::NoDevice { index })
    }

    fn set_bus_master(&self, index: usize, _: bool) -> Result<(), PciError> {
        Err(PciError::NoDevice { index })
    }
}
