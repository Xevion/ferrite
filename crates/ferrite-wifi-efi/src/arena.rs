//! The kernel arena: firmware pages below 4 GiB, so 32-bit DMA works and the bus address is the
//! address itself.

use core::ptr::NonNull;

use ferrite_hal::BusAddr;
use ferrite_lkpi::mem::{ARENA_ALIGN, Arena, ArenaError, PAGE_SIZE};
use snafu::Snafu;
use uefi::boot::{self, AllocateType, MemoryType};

/// The highest byte an arena may occupy.
const BELOW_4GIB: u64 = 0xffff_ffff;

#[derive(Debug, Snafu)]
pub enum ArenaAllocError {
    #[snafu(display("the firmware has no {size}-byte range below 4 GiB for the kernel arena"))]
    Allocate { size: usize, source: uefi::Error },
    #[snafu(display("the kernel arena is unusable"))]
    Arena { source: ArenaError },
}

/// The firmware pages behind an arena, to hand back when the program ends.
#[derive(Debug)]
pub struct ArenaPages {
    base: NonNull<u8>,
    pages: usize,
}

impl ArenaPages {
    /// Return the pages to the firmware.
    ///
    /// # Safety
    /// Nothing may touch the arena afterwards.
    pub unsafe fn release(self) {
        // SAFETY: the pages are the allocation this value describes, freed once.
        if let Err(error) = unsafe { boot::free_pages(self.base, self.pages) } {
            log::warn!("freeing the kernel arena failed: {error}");
        }
    }
}

/// Allocate an arena of `size` bytes (a multiple of the page size), 4 MiB aligned like its bus address.
///
/// Firmware pages are only page aligned, so one allocation is made large enough to align inside and
/// the slack at both ends goes straight back to the firmware. The memory is zeroed, as an anonymous
/// mapping is, since the memmap and allocator state start from it.
pub fn allocate(size: usize) -> Result<(Arena, ArenaPages), ArenaAllocError> {
    let total = size + ARENA_ALIGN;
    let raw = boot::allocate_pages(AllocateType::MaxAddress(BELOW_4GIB), MemoryType::LOADER_DATA, total / PAGE_SIZE)
        .map_err(|source| ArenaAllocError::Allocate { size: total, source })?;
    let head = raw.as_ptr().addr().next_multiple_of(ARENA_ALIGN) - raw.as_ptr().addr();
    let tail = total - head - size;
    // SAFETY: `head` and `head + size` lie inside the allocation, which has `total` bytes.
    let (base, end) = unsafe { (raw.add(head), raw.add(head + size)) };
    // SAFETY: both ranges are inside the allocation, not shared, and freed once each.
    unsafe {
        if head > 0 {
            trim(raw, head);
        }
        if tail > 0 {
            trim(end, tail);
        }
    }
    // SAFETY: `size` bytes from `base` are whole pages of the allocation, owned for the rest of the program.
    unsafe { base.as_ptr().write_bytes(0, size) };
    let bus = BusAddr::new(base.as_ptr().addr() as u64);
    // SAFETY: the range is writable and unused by anything else; the firmware's memory map is the identity
    // map, so a device reaches it at its own address.
    let arena = unsafe { Arena::new(base, bus, size) }.map_err(|source| ArenaAllocError::Arena { source })?;
    Ok((arena, ArenaPages { base, pages: size / PAGE_SIZE }))
}

/// Give `bytes` from `start` back to the firmware.
///
/// # Safety
/// The range must be whole pages of an allocation, not used by anything.
unsafe fn trim(start: NonNull<u8>, bytes: usize) {
    // SAFETY: the caller vouches for the range.
    if let Err(error) = unsafe { boot::free_pages(start, bytes / PAGE_SIZE) } {
        log::warn!("returning {bytes} bytes of arena slack at {start:p} failed: {error}");
    }
}
