//! Platform traits and address, time and register types shared by the drivers
//! and their backends.
//!
//! Drivers are written against these traits; the UEFI backend lives in
//! `ferrite-efi` and the host test doubles in `ferrite-testing`.

#![no_std]

#[cfg(test)]
extern crate std;

mod addr;
mod reg;
mod time;
mod wall;

use core::fmt;
use core::ptr::NonNull;
use core::time::Duration;

pub use addr::{BusAddr, BusAddr32, PciAddress, PhysAddr};
pub use reg::{DmaPtrReg, MmioExt, ReadOnly, ReadWrite, Readable, Reg, RegValue, Rw1c, Rw1cWrite};
pub use time::{Instant, PageCount};
pub use wall::{Correction, MAX_RATE_PPM, Offset, UnixTime, WallClock};

/// Size of one DMA page in bytes.
pub const PAGE_SIZE: usize = 4096;

/// A failed platform service call, carrying the backend's status code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlatformError {
    code: usize,
}

impl PlatformError {
    #[must_use]
    pub const fn new(code: usize) -> Self {
        Self { code }
    }

    #[must_use]
    pub const fn code(self) -> usize {
        self.code
    }
}

impl fmt::Display for PlatformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "platform status {:#x}", self.code)
    }
}

impl core::error::Error for PlatformError {}

/// 32-bit memory-mapped register access by CPU-physical address.
///
/// [`MmioExt`] layers typed [`Reg`] access on top of this.
pub trait Mmio {
    fn try_read32(&mut self, addr: PhysAddr) -> Result<u32, PlatformError>;

    fn try_write32(&mut self, addr: PhysAddr, value: u32) -> Result<(), PlatformError>;

    /// Hot-path read for polling: a failed access reads as all ones, like a master abort.
    fn read32(&mut self, addr: PhysAddr) -> u32 {
        self.try_read32(addr).unwrap_or(u32::MAX)
    }

    /// Hot-path write for doorbells and acknowledgements: a failed access is dropped.
    ///
    /// Use [`Mmio::try_write32`] wherever a lost write would leave the device misprogrammed.
    fn write32(&mut self, addr: PhysAddr, value: u32) {
        // Best effort by contract; a failure resurfaces as an all-ones read on the next poll.
        let _ = self.try_write32(addr, value);
    }
}

/// PCI configuration space access for any function behind this backend.
pub trait PciConfig {
    fn cfg_read8(&mut self, dev: PciAddress, offset: u8) -> Result<u8, PlatformError>;
    fn cfg_read16(&mut self, dev: PciAddress, offset: u8) -> Result<u16, PlatformError>;
    fn cfg_read32(&mut self, dev: PciAddress, offset: u8) -> Result<u32, PlatformError>;
    fn cfg_write8(&mut self, dev: PciAddress, offset: u8, value: u8) -> Result<(), PlatformError>;
    fn cfg_write16(&mut self, dev: PciAddress, offset: u8, value: u16) -> Result<(), PlatformError>;
    fn cfg_write32(&mut self, dev: PciAddress, offset: u8, value: u32) -> Result<(), PlatformError>;
}

/// One contiguous block of device-visible memory below 4 GiB, released when dropped.
///
/// # Safety
/// Implementors guarantee that, for as long as the value lives, `cpu_ptr` is
/// valid for volatile reads and writes of `pages` pages, page aligned and was
/// zeroed at allocation; that the device reaches byte `i` at `bus_addr + i`
/// with every such address below 4 GiB; and that CPU and device views are
/// coherent without explicit flushes.
pub unsafe trait DmaRegion {
    fn cpu_ptr(&self) -> NonNull<u8>;

    fn bus_addr(&self) -> BusAddr32;

    fn pages(&self) -> PageCount;
}

/// Allocator for DMA memory the device can address with 32-bit pointers.
pub trait DmaAlloc {
    type Region: DmaRegion;

    /// Allocate `pages` zeroed, contiguous pages lying entirely below 4 GiB.
    fn alloc_dma32(&mut self, pages: PageCount) -> Result<Self::Region, PlatformError>;

    /// Whether an IOMMU may translate device addresses this allocator does not map.
    fn iommu_active(&mut self) -> bool;
}

/// A monotonic clock.
pub trait Clock {
    fn now(&self) -> Instant;

    /// Time since `since`, zero if `since` is in the future.
    fn elapsed(&self, since: Instant) -> Duration {
        self.now() - since
    }
}

/// A busy-wait delay.
pub trait Delay {
    fn delay(&self, duration: Duration);
}
