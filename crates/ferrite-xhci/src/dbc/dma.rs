//! DMA memory for the DbC. UEFI maps all RAM 1:1 and x86 DMA is cache
//! coherent, so physical pages from `allocate_pages` are directly usable as bus
//! addresses unless an IOMMU is active (detected, not handled).

use core::ptr::{self, NonNull};

use uefi::boot::{self, AllocateType, SearchType};
use uefi::mem::memory_map::MemoryType;
use uefi::{Guid, guid};

pub const PAGE: usize = 4096;
/// Highest bus address the block may end at; keeps every pointer 32-bit clean.
const MAX_ADDR: u64 = 0xffff_ffff;
const BOUNDARY: u64 = 0x1_0000;

pub const EDKII_IOMMU_PROTOCOL: Guid = guid!("4e939de9-d948-4b0f-88ed-e6e1ce517c1e");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaError {
    Alloc,
    OutOfSpace,
}

/// Number of handles carrying the EDK2 IOMMU protocol (0 means identity DMA is safe).
#[must_use]
pub fn iommu_handle_count() -> usize {
    boot::locate_handle_buffer(SearchType::ByProtocol(&EDKII_IOMMU_PROTOCOL)).map_or(0, |h| h.len())
}

/// A carved region of a [`DmaBlock`]; valid while the block lives.
#[derive(Clone, Copy, Debug)]
pub struct DmaSlice {
    ptr: *mut u8,
    phys: u64,
    len: usize,
}

impl DmaSlice {
    #[must_use]
    pub const fn ptr(&self) -> *mut u8 {
        self.ptr
    }

    #[must_use]
    pub const fn phys(&self) -> u64 {
        self.phys
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn zero(&self) {
        // SAFETY: the slice lies inside a live allocation.
        unsafe { ptr::write_bytes(self.ptr, 0, self.len) }
    }

    /// Store `val` at byte offset `off` within the slice.
    pub fn write<T: Copy>(&self, off: usize, val: T) {
        assert!(off + size_of::<T>() <= self.len);
        // SAFETY: bounds checked above; callers keep `off` aligned for `T`.
        unsafe { ptr::write_volatile(self.ptr.add(off).cast::<T>(), val) }
    }

    /// Copy `len` bytes at byte offset `off` into `out`.
    pub fn read_into(&self, off: usize, out: &mut [u8]) {
        assert!(off + out.len() <= self.len);
        for (i, b) in out.iter_mut().enumerate() {
            // SAFETY: bounds checked above.
            *b = unsafe { ptr::read_volatile(self.ptr.add(off + i)) };
        }
    }
}

/// Zeroed pages below 4 GiB, freed on drop. The caller must stop the device
/// before dropping.
pub struct DmaBlock {
    ptr: NonNull<u8>,
    phys: u64,
    pages: usize,
    used: usize,
}

impl DmaBlock {
    pub fn alloc(pages: usize) -> Result<Self, DmaError> {
        let ptr = boot::allocate_pages(AllocateType::MaxAddress(MAX_ADDR), MemoryType::BOOT_SERVICES_DATA, pages)
            .map_err(|_| DmaError::Alloc)?;
        let phys = ptr.as_ptr() as u64;
        if phys + (pages * PAGE) as u64 - 1 > MAX_ADDR {
            // SAFETY: just allocated with the same page count.
            let _ = unsafe { boot::free_pages(ptr, pages) };
            return Err(DmaError::Alloc);
        }
        // SAFETY: the allocation spans `pages * PAGE` bytes.
        unsafe { ptr::write_bytes(ptr.as_ptr(), 0, pages * PAGE) };
        Ok(Self { ptr, phys, pages, used: 0 })
    }

    #[must_use]
    pub const fn phys(&self) -> u64 {
        self.phys
    }

    /// Carve the next `len` bytes at `align`, never straddling a 64 KiB boundary.
    pub fn carve(&mut self, len: usize, align: usize) -> Result<DmaSlice, DmaError> {
        let off = carve_offset(self.phys, self.used, len, align);
        if off + len > self.pages * PAGE {
            return Err(DmaError::OutOfSpace);
        }
        self.used = off + len;
        Ok(DmaSlice {
            // SAFETY: off + len is inside the allocation.
            ptr: unsafe { self.ptr.as_ptr().add(off) },
            phys: self.phys + off as u64,
            len,
        })
    }
}

impl Drop for DmaBlock {
    fn drop(&mut self) {
        // SAFETY: allocated in `alloc` with the same page count.
        let _ = unsafe { boot::free_pages(self.ptr, self.pages) };
    }
}

/// Offset for the next region: aligned, and moved to the next 64 KiB boundary if
/// it would otherwise cross one. `len` must not exceed 64 KiB.
#[must_use]
pub fn carve_offset(base_phys: u64, used: usize, len: usize, align: usize) -> usize {
    let mut off = used.next_multiple_of(align);
    let start = base_phys + off as u64;
    let end = start + len as u64 - 1;
    if start / BOUNDARY != end / BOUNDARY {
        off += (BOUNDARY - start % BOUNDARY) as usize;
    }
    off
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carve_keeps_alignment() {
        assert_eq!(carve_offset(0x10000, 100, 64, 64), 128);
        assert_eq!(carve_offset(0x10000, 0, 4096, 4096), 0);
    }

    #[test]
    fn carve_skips_64k_boundary() {
        // Block starts 8 KiB below a 64 KiB boundary; a 16 KiB region must move past it.
        let base = 0x2_0000 - 0x2000;
        assert_eq!(carve_offset(base, 0, 0x4000, 4096), 0x2000);
        assert_eq!(carve_offset(base, 0, 0x2000, 4096), 0);
    }
}
