//! The page structs the C makes for the pages it looks up, by page address. The structs themselves
//! are allocated and filled by the glue `lx_virt_to_page`, which knows their layout; this map only
//! remembers which struct belongs to which page, for the program's lifetime.

use alloc::collections::BTreeMap;
use core::cell::UnsafeCell;
use core::ffi::c_void;

struct Pages(UnsafeCell<BTreeMap<usize, usize>>);

// SAFETY: the C runs on one core without preemption, and each call below finishes before another
// can start, so the map is never touched from two places at once.
unsafe impl Sync for Pages {}

static PAGES: Pages = Pages(UnsafeCell::new(BTreeMap::new()));

/// The page struct recorded for the page at `virt`, or null.
#[unsafe(no_mangle)]
pub extern "sysv64" fn lx_page_lookup(virt: usize) -> *mut c_void {
    // SAFETY: as for `Sync` above; the borrow ends before returning.
    let pages = unsafe { &*PAGES.0.get() };
    pages.get(&virt).map_or(core::ptr::null_mut(), |&page| core::ptr::with_exposed_provenance_mut(page))
}

/// Record `page` as the page struct of the page at `virt`.
#[unsafe(no_mangle)]
pub extern "sysv64" fn lx_page_insert(virt: usize, page: *mut c_void) {
    // SAFETY: as for `Sync` above; the borrow ends before returning.
    let pages = unsafe { &mut *PAGES.0.get() };
    pages.insert(virt, page.expose_provenance());
}
