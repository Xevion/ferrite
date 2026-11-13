//! The kernel arena: firmware pages below 4 GiB, so 32-bit DMA works.
//!
//! Without DMA protection the bus address is the address itself; with it, the pages come from the
//! card's PCI I/O `AllocateBuffer` and one `Map` as a common buffer gives their bus address.

use core::ffi::c_void;
use core::ptr::NonNull;

use ferrite_hal::BusAddr;
use ferrite_lkpi::mem::{ARENA_ALIGN, Arena, ArenaError, PAGE_SIZE};
use snafu::Snafu;
use uefi::boot::{self, AllocateType, MemoryType};

use crate::pci::{MAP_COMMON_BUFFER, PciIo};

/// `AllocateAnyPages`, which PCI I/O `AllocateBuffer` requires.
const ALLOCATE_ANY_PAGES: u32 = 0;
/// The highest byte an arena may occupy.
const BELOW_4GIB: u64 = 0xffff_ffff;

#[derive(Debug, Snafu)]
pub enum ArenaAllocError {
    #[snafu(display("the firmware has no {size}-byte range below 4 GiB for the kernel arena"))]
    Allocate { size: usize, source: uefi::Error },
    #[snafu(display("the kernel arena is unusable"))]
    Arena { source: ArenaError },
    #[snafu(display("PCI I/O AllocateBuffer of {pages} pages failed: {status:?}"))]
    Buffer { pages: usize, status: uefi::Status },
    #[snafu(display("PCI I/O Map of {size:#x} bytes failed: {status:?}"))]
    Map { size: usize, status: uefi::Status },
    #[snafu(display("PCI I/O Map covered {mapped:#x} of {size:#x} bytes"))]
    Partial { size: usize, mapped: usize },
}

/// The firmware memory behind an arena, to hand back when the program ends.
pub enum ArenaPages {
    Pages { base: NonNull<u8>, pages: usize },
    Mapped { io: &'static PciIo, buffer: NonNull<u8>, pages: usize, mapping: *mut c_void },
}

impl ArenaPages {
    /// Return the memory to the firmware, unmapping it from the card first.
    ///
    /// # Safety
    /// Nothing may touch the arena afterwards, the card included.
    pub unsafe fn release(self) {
        match self {
            Self::Pages { base, pages } => {
                // SAFETY: the pages are the allocation this value describes, freed once.
                if let Err(error) = unsafe { boot::free_pages(base, pages) } {
                    log::warn!("freeing the kernel arena failed: {error}");
                }
            },
            Self::Mapped { io, buffer, pages, mapping } => {
                // SAFETY: the mapping and buffer are the ones `allocate_mapped` made, each released once.
                let unmapped = unsafe { (io.unmap)(io, mapping) };
                // SAFETY: as above.
                let freed = unsafe { (io.free_buffer)(io, pages, buffer.as_ptr().cast()) };
                if unmapped.is_error() || freed.is_error() {
                    log::warn!("releasing the mapped kernel arena: unmap {unmapped:?}, free {freed:?}");
                }
            },
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
    Ok((arena, ArenaPages::Pages { base, pages: size / PAGE_SIZE }))
}

/// Allocate an arena of `size` bytes through the card's PCI I/O and map it for the card as a common
/// buffer, so the firmware's IOMMU lets the card reach it.
///
/// The buffer is over-allocated by 4 MiB so a 4 MiB-aligned window fits; the slack stays allocated, as
/// PCI I/O frees only whole buffers. Only the window is mapped. The arena needs its bus address congruent
/// with its address modulo 4 MiB, which an identity-mapping IOMMU gives; any other offset is refused.
pub fn allocate_mapped(io: &'static PciIo, size: usize) -> Result<(Arena, ArenaPages), ArenaAllocError> {
    let pages = (size + ARENA_ALIGN) / PAGE_SIZE;
    let mut host: *mut c_void = core::ptr::null_mut();
    // SAFETY: `host` is writable; no attributes, so the buffer lies below 4 GiB.
    let status = unsafe {
        (io.allocate_buffer)(io, ALLOCATE_ANY_PAGES, MemoryType::BOOT_SERVICES_DATA.0, pages, &raw mut host, 0)
    };
    let buffer = match NonNull::new(host.cast::<u8>()) {
        Some(buffer) if status.is_success() => buffer,
        _ => return Err(ArenaAllocError::Buffer { pages, status }),
    };
    let release = |mapping: Option<*mut c_void>| {
        // SAFETY: undoes the map and allocation above on a failed path, once each.
        unsafe {
            if let Some(mapping) = mapping {
                let _ = (io.unmap)(io, mapping);
            }
            let _ = (io.free_buffer)(io, pages, buffer.as_ptr().cast());
        }
    };
    let head = buffer.as_ptr().addr().next_multiple_of(ARENA_ALIGN) - buffer.as_ptr().addr();
    // SAFETY: `head + size` bytes lie inside the `size + ARENA_ALIGN`-byte buffer.
    let base = unsafe { buffer.add(head) };
    let (mut mapped, mut bus, mut mapping) = (size, 0_u64, core::ptr::null_mut());
    // SAFETY: the window is part of a buffer PCI I/O allocated; all outputs are writable.
    let status = unsafe {
        (io.map)(io, MAP_COMMON_BUFFER, base.as_ptr().cast(), &raw mut mapped, &raw mut bus, &raw mut mapping)
    };
    if status.is_error() {
        release(None);
        return Err(ArenaAllocError::Map { size, status });
    }
    if mapped < size {
        release(Some(mapping));
        return Err(ArenaAllocError::Partial { size, mapped });
    }
    // SAFETY: the window is ours for the rest of the program.
    unsafe { base.as_ptr().write_bytes(0, size) };
    log::info!("kernel arena mapped for the card: host {:p}, bus {bus:#x}, {} MiB", base, size >> 20);
    // SAFETY: writable, unused elsewhere, and visible to the card at `bus` through the mapping.
    match unsafe { Arena::new(base, BusAddr::new(bus), size) } {
        Ok(arena) => Ok((arena, ArenaPages::Mapped { io, buffer, pages, mapping })),
        Err(source) => {
            release(Some(mapping));
            Err(ArenaAllocError::Arena { source })
        },
    }
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
