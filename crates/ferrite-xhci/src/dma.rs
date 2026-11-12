//! One DMA block carved into the regions the DbC needs.

use core::mem;
use core::ptr::{self, NonNull};

use ferrite_hal::{BusAddr32, DmaRegion, PageCount};
use snafu::{OptionExt, ensure};

use crate::error::{Above4GSnafu, DmaError, OutOfSpaceSnafu};

/// Regions never straddle this boundary, so each is one contiguous DMA burst target.
const BOUNDARY: u64 = 0x1_0000;

/// Pages of slack a region of `pages` pages may need to dodge a 64 KiB boundary.
///
/// Regions are page aligned and the boundary is a page multiple, so a region
/// that would cross it moves forward by fewer than its own page count.
#[must_use]
pub const fn boundary_slack(pages: PageCount) -> PageCount {
    PageCount::new(pages.get().saturating_sub(1))
}

/// A backend allocation plus a bump pointer for carving it.
pub struct DmaBlock<R: DmaRegion> {
    region: R,
    used: usize,
}

impl<R: DmaRegion> DmaBlock<R> {
    /// Take ownership of `region` after checking it is device addressable with 32-bit pointers.
    pub fn new(region: R) -> Result<Self, DmaError> {
        let bus = region.bus_addr();
        let len = region.pages().bytes();
        let last = u32::try_from(len.saturating_sub(1)).ok().and_then(|l| bus.checked_add(l));
        ensure!(last.is_some(), Above4GSnafu { addr: bus.to_bus(), len });
        Ok(Self { region, used: 0 })
    }

    #[must_use]
    pub fn bus_addr(&self) -> BusAddr32 {
        self.region.bus_addr()
    }

    #[must_use]
    pub fn pages(&self) -> PageCount {
        self.region.pages()
    }

    /// Carve the next `len` bytes at `align`, never straddling a 64 KiB boundary.
    pub fn carve(&mut self, len: usize, align: usize) -> Result<DmaSlice, DmaError> {
        let base = self.region.bus_addr();
        let bytes = self.region.pages().bytes();
        let off = carve_offset(base, self.used, len, align)
            .filter(|off| off.checked_add(len).is_some_and(|end| end <= bytes))
            .context(OutOfSpaceSnafu { len, align })?;
        let bus = u32::try_from(off)
            .ok()
            .and_then(|off| base.checked_add(off))
            .context(Above4GSnafu { addr: base.to_bus(), len: off })?;
        self.used = off + len;
        // SAFETY: `off + len` lies inside the region, which stays allocated while `self` lives.
        let ptr = unsafe { self.region.cpu_ptr().add(off) };
        Ok(DmaSlice { ptr, bus, len })
    }

    /// Give up the memory without freeing it, for when the device may still be using it.
    pub fn into_leaked(self) {
        mem::forget(self.region);
    }
}

/// Offset for the next region: aligned, and moved to the next 64 KiB boundary if
/// it would otherwise cross one. `len` must be nonzero and at most 64 KiB, and
/// `align` a power of two. `None` when the offset overflows.
#[must_use]
pub fn carve_offset(base: BusAddr32, used: usize, len: usize, align: usize) -> Option<usize> {
    let off = used.checked_next_multiple_of(align)?;
    let start = u64::from(base.get()).checked_add(u64::try_from(off).ok()?)?;
    let end = start.checked_add(u64::try_from(len).ok()?.checked_sub(1)?)?;
    if start / BOUNDARY == end / BOUNDARY {
        return Some(off);
    }
    off.checked_add(usize::try_from(BOUNDARY - start % BOUNDARY).ok()?)
}

/// A carved region of a [`DmaBlock`]. It holds a raw pointer and no lifetime, so
/// it stays crate-private: the owner keeps the block alive for as long as any
/// slice of it is in use.
#[derive(Clone, Copy, Debug)]
pub struct DmaSlice {
    ptr: NonNull<u8>,
    bus: BusAddr32,
    len: usize,
}

impl DmaSlice {
    /// # Safety
    /// `ptr` must be valid for volatile reads and writes of `len` bytes for as
    /// long as the slice or anything built from it is used, and the device must
    /// see byte `i` at `bus + i`.
    #[cfg(test)]
    pub const unsafe fn from_raw(ptr: NonNull<u8>, bus: BusAddr32, len: usize) -> Self {
        Self { ptr, bus, len }
    }

    pub const fn ptr(&self) -> NonNull<u8> {
        self.ptr
    }

    pub const fn bus(&self) -> BusAddr32 {
        self.bus
    }

    /// Bus address of byte `off`.
    ///
    /// # Panics
    /// If `off` is past the end of the slice.
    pub fn bus_at(&self, off: usize) -> BusAddr32 {
        assert!(off <= self.len, "offset past the DMA slice");
        // A slice never reaches 4 GiB, so an in-bounds offset stays below it.
        self.bus.checked_add(off as u32).unwrap_or_else(|| panic!("DMA slice reaches 4 GiB"))
    }

    pub const fn len(&self) -> usize {
        self.len
    }

    pub fn zero(&self) {
        // SAFETY: the slice is valid for `len` bytes by construction.
        unsafe { ptr::write_bytes(self.ptr.as_ptr(), 0, self.len) }
    }

    /// Load a `T` from byte offset `off`, as one volatile access.
    ///
    /// # Panics
    /// If `off` is out of bounds or misaligned for `T`.
    #[expect(clippy::disallowed_methods, reason = "the DMA slice is the one home for volatile DMA access")]
    pub fn read<T: Copy>(&self, off: usize) -> T {
        assert!(off.checked_add(size_of::<T>()).is_some_and(|end| end <= self.len), "DMA read out of bounds");
        // SAFETY: in bounds per the check above.
        let src = unsafe { self.ptr.add(off) }.cast::<T>();
        assert!(src.is_aligned(), "DMA read misaligned for its type");
        // SAFETY: in bounds and aligned per the checks above; the device may write
        // concurrently, which a volatile access tolerates.
        unsafe { ptr::read_volatile(src.as_ptr()) }
    }

    /// Store `val` at byte offset `off` within the slice.
    ///
    /// # Panics
    /// If `off` is out of bounds or misaligned for `T`.
    #[expect(clippy::disallowed_methods, reason = "the DMA slice is the one home for volatile DMA access")]
    pub fn write<T: Copy>(&self, off: usize, val: T) {
        assert!(off.checked_add(size_of::<T>()).is_some_and(|end| end <= self.len), "DMA write out of bounds");
        // SAFETY: in bounds per the check above.
        let dst = unsafe { self.ptr.add(off) }.cast::<T>();
        assert!(dst.is_aligned(), "DMA write misaligned for its type");
        // SAFETY: in bounds and aligned per the checks above.
        unsafe { ptr::write_volatile(dst.as_ptr(), val) }
    }

    /// Copy `out.len()` bytes from byte offset `off` into `out`.
    ///
    /// # Panics
    /// If the range is out of bounds.
    pub fn read_into(&self, off: usize, out: &mut [u8]) {
        for (i, b) in out.iter_mut().enumerate() {
            *b = self.read(off + i);
        }
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;
    use ferrite_hal::{DmaAlloc, PAGE_SIZE};
    use ferrite_testing::Sim;
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn carve_skips_64k_boundary() {
        // Block starts 8 KiB below a 64 KiB boundary; a 16 KiB region must move past it.
        let base = BusAddr32::from_u32(0x2_0000 - 0x2000);
        check!(carve_offset(base, 0, 0x4000, 4096) == Some(0x2000));
        check!(carve_offset(base, 0, 0x2000, 4096) == Some(0));
    }

    proptest! {
        #[test]
        fn carve_offset_never_crosses_64k(
            base_page in 0u32..64,
            used in 0usize..0x2_0000,
            len in 1usize..=0x1_0000,
            align_shift in 0u32..=12,
        ) {
            let base = base_page * PAGE_SIZE as u32;
            let align = 1usize << align_shift;
            let off = carve_offset(BusAddr32::from_u32(base), used, len, align).expect("small inputs never overflow");
            let start = u64::from(base) + off as u64;
            let end = start + len as u64 - 1;
            prop_assert!(off >= used, "monotonic");
            prop_assert_eq!(off % align, 0, "aligned");
            prop_assert_eq!(start / BOUNDARY, end / BOUNDARY, "inside one 64 KiB window");
        }
    }

    #[test]
    fn block_above_4g_is_rejected() {
        let sim = Sim::new();
        sim.set_dma_base(0xffff_f000);
        let region = sim.dma().alloc_dma32(PageCount::new(2)).expect("sim allocation");
        check!(DmaBlock::new(region).is_err());
    }

    #[test]
    fn misaligned_write_panics() {
        let mut mem = std::vec![0u64; 4];
        // SAFETY: `mem` outlives the slice and is 32 bytes long.
        let slice = unsafe { DmaSlice::from_raw(NonNull::from(&mut mem[0]).cast(), BusAddr32::from_u32(0), 32) };
        let r = std::panic::catch_unwind(|| slice.write::<u32>(2, 7));
        check!(r.is_err());
    }
}
