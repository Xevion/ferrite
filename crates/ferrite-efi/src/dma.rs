//! DMA memory from boot-services page allocation, as the `ferrite-hal` DMA backend.
//!
//! UEFI maps all RAM 1:1 and x86 DMA is cache coherent, so pages below 4 GiB
//! are directly usable as bus addresses unless an IOMMU is active, which is
//! detected and reported but not handled.

use core::ptr::{self, NonNull};

use ferrite_hal::{BusAddr, BusAddr32, DmaAlloc, DmaRegion, PageCount, PlatformError};
use uefi::boot::{self, AllocateType, SearchType};
use uefi::mem::memory_map::MemoryType;
use uefi::{Guid, Status, guid};

use crate::pci::platform_error;

/// Highest address an allocation may reach, so every bus pointer is 32-bit clean.
const MAX_ADDR: u64 = 0xffff_ffff;

pub const EDKII_IOMMU_PROTOCOL: Guid = guid!("4e939de9-d948-4b0f-88ed-e6e1ce517c1e");

/// Number of handles carrying the EDK2 IOMMU protocol (0 means identity DMA is safe).
pub fn iommu_handle_count() -> uefi::Result<usize> {
    match boot::locate_handle_buffer(SearchType::ByProtocol(&EDKII_IOMMU_PROTOCOL)) {
        Ok(handles) => Ok(handles.len()),
        Err(e) if e.status() == Status::NOT_FOUND => Ok(0),
        Err(e) => Err(e),
    }
}

/// Zeroed boot-services pages below 4 GiB, freed on drop.
pub struct BootPages {
    ptr: NonNull<u8>,
    bus: BusAddr32,
    pages: PageCount,
}

// SAFETY: `allocate_pages` returned `pages` page-aligned pages which `alloc_dma32`
// zeroed and checked to lie below 4 GiB; they stay allocated until drop, RAM is
// identity mapped, and x86 DMA is coherent.
unsafe impl DmaRegion for BootPages {
    fn cpu_ptr(&self) -> NonNull<u8> {
        self.ptr
    }

    fn bus_addr(&self) -> BusAddr32 {
        self.bus
    }

    fn pages(&self) -> PageCount {
        self.pages
    }
}

impl Drop for BootPages {
    fn drop(&mut self) {
        // SAFETY: allocated in `alloc_dma32` with the same page count, and only freed here.
        if let Err(e) = unsafe { boot::free_pages(self.ptr, self.pages.get()) } {
            log::warn!("freeing {} DMA pages at {} failed: {e}", self.pages.get(), self.bus);
        }
    }
}

/// [`DmaAlloc`] through boot-services `AllocatePages`.
#[derive(Clone, Copy, Debug, Default)]
pub struct BootServicesDma;

impl DmaAlloc for BootServicesDma {
    type Region = BootPages;

    fn alloc_dma32(&mut self, pages: PageCount) -> Result<BootPages, PlatformError> {
        let ptr = boot::allocate_pages(AllocateType::MaxAddress(MAX_ADDR), MemoryType::BOOT_SERVICES_DATA, pages.get())
            .map_err(|e| platform_error(&e))?;
        let Some(bus) = BusAddr32::new(BusAddr::new(ptr.as_ptr() as u64)) else {
            // SAFETY: just allocated with the same page count.
            if let Err(e) = unsafe { boot::free_pages(ptr, pages.get()) } {
                log::warn!("freeing a misplaced DMA allocation failed: {e}");
            }
            return Err(PlatformError::new(Status::OUT_OF_RESOURCES.0));
        };
        // SAFETY: the allocation spans `pages` pages.
        unsafe { ptr::write_bytes(ptr.as_ptr(), 0, pages.bytes()) };
        Ok(BootPages { ptr, bus, pages })
    }

    /// Fails closed: if the IOMMU protocol lookup itself fails, assume one is there.
    fn iommu_active(&mut self) -> bool {
        match iommu_handle_count() {
            Ok(n) => n > 0,
            Err(e) => {
                log::warn!("IOMMU protocol lookup failed ({e}); assuming an IOMMU is active");
                true
            },
        }
    }
}
