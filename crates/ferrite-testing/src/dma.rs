//! A DMA arena with fake bus addresses, tracking which regions were freed.

use std::alloc::{self, Layout};
use std::ptr::NonNull;

use ferrite_hal::{BusAddr32, DmaAlloc, DmaRegion, PAGE_SIZE, PageCount, PlatformError};

use crate::{INJECTED_FAILURE, Sim};

/// Default bus address of the first allocation.
const FIRST_BUS: u64 = 0x1000_0000;

pub struct Region {
    pub bus: u64,
    pub ptr: NonNull<u8>,
    pub layout: Layout,
    /// The driver dropped its handle. The memory stays allocated until the
    /// `Sim` goes, so a device access after free is recorded instead of being UB.
    pub freed: bool,
}

pub struct Arena {
    pub regions: Vec<Region>,
    pub next: u64,
    pub fail: bool,
    pub iommu: bool,
    pub violations: usize,
}

impl Default for Arena {
    fn default() -> Self {
        Self { regions: Vec::new(), next: FIRST_BUS, fail: false, iommu: false, violations: 0 }
    }
}

impl Arena {
    /// CPU pointer for `len` bytes at `bus`, or `None` (counted as a violation)
    /// if that is not inside one live region.
    pub fn translate(&mut self, bus: u64, len: usize) -> Option<NonNull<u8>> {
        let found = self.regions.iter().find(|r| bus >= r.bus && bus + len as u64 <= r.bus + r.layout.size() as u64);
        match found {
            Some(r) if !r.freed => {
                // SAFETY: the offset lies inside the region's allocation.
                Some(unsafe { r.ptr.add(usize::try_from(bus - r.bus).ok()?) })
            },
            _ => {
                self.violations += 1;
                None
            },
        }
    }
}

impl Drop for Arena {
    fn drop(&mut self) {
        for r in &self.regions {
            // SAFETY: allocated in `alloc_dma32` with this layout and freed only here.
            unsafe { alloc::dealloc(r.ptr.as_ptr(), r.layout) };
        }
    }
}

/// [`DmaAlloc`] over the shared [`Sim`].
pub struct SimDma(Sim);

impl SimDma {
    pub(crate) const fn new(sim: Sim) -> Self {
        Self(sim)
    }
}

impl DmaAlloc for SimDma {
    type Region = SimRegion;

    fn alloc_dma32(&mut self, pages: PageCount) -> Result<SimRegion, PlatformError> {
        let mut st = self.0.0.borrow_mut();
        if st.dma.fail || pages.get() == 0 {
            return Err(PlatformError::new(INJECTED_FAILURE));
        }
        let layout =
            Layout::from_size_align(pages.bytes(), PAGE_SIZE).map_err(|_| PlatformError::new(INJECTED_FAILURE))?;
        // SAFETY: the layout has a nonzero size.
        let ptr = NonNull::new(unsafe { alloc::alloc_zeroed(layout) }).ok_or(PlatformError::new(INJECTED_FAILURE))?;
        let bus = st.dma.next;
        st.dma.next += pages.bytes() as u64;
        st.dma.regions.push(Region { bus, ptr, layout, freed: false });
        let index = st.dma.regions.len() - 1;
        Ok(SimRegion { sim: self.0.clone(), index, ptr, bus: BusAddr32::from_u32(bus as u32), pages })
    }

    fn iommu_active(&mut self) -> bool {
        self.0.0.borrow().dma.iommu
    }
}

/// One simulated DMA allocation; dropping it marks the region freed.
pub struct SimRegion {
    sim: Sim,
    index: usize,
    ptr: NonNull<u8>,
    bus: BusAddr32,
    pages: PageCount,
}

// SAFETY: the arena keeps the zeroed, page-aligned allocation alive until the
// `Sim` drops, and this handle holds the `Sim`; the device model reaches byte
// `i` at `bus + i` through `Arena::translate`.
unsafe impl DmaRegion for SimRegion {
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

impl Drop for SimRegion {
    fn drop(&mut self) {
        self.sim.0.borrow_mut().dma.regions[self.index].freed = true;
    }
}
