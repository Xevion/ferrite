//! Kernel memory: the arena the app hands over at start-up, its memmap, and the page allocator's buddy.
//!
//! The C page allocator (shadow `mm/page_alloc.c`) draws blocks from the buddy here; the slab layer
//! and kmalloc are C on top of those pages (shadow `mm/slab_common.c`).
//!
//! Addresses follow Linux's flat model. The arena's bus address is its "physical" address, so
//! `__pa(virt)` is `virt - page_offset_base` and that is also the address a device DMAs to: the arena's
//! IOVA in the lab, the identity address under UEFI. The memmap is one `struct page` per arena page,
//! placed at the arena's front, with `vmemmap_base` set so that the stock `vmemmap + pfn` lands in it.
//! The C sizes it (`sizeof(struct page)`) on its first allocation through `lx_mem_setup`.
//!
//! Memory use before `install` stops the program like a stub hit.

use core::cell::UnsafeCell;
use core::ffi::{CStr, c_char, c_uint, c_ulong, c_void};
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use ferrite_hal::BusAddr;
use snafu::Snafu;

use crate::buddy::{Buddy, MAX_ORDER};
use crate::trace_and_stop;

pub const PAGE_SIZE: usize = 4096;
const PAGE_SHIFT: u32 = 12;
/// Arena base and bus address must agree modulo the largest block, so a block aligned in bus
/// addresses is aligned the same in virtual ones.
pub const ARENA_ALIGN: usize = PAGE_SIZE << MAX_ORDER;
/// x86-64 Linux's `__START_KERNEL_map`.
const START_KERNEL_MAP: usize = 0xffff_ffff_8000_0000;

/// `__va`/`__pa` offset: virtual minus bus address of the arena.
#[unsafe(no_mangle)]
pub static page_offset_base: AtomicUsize = AtomicUsize::new(0);
/// Set so `__pa_symbol` agrees with `__pa`; the shim has no kernel-image mapping.
#[unsafe(no_mangle)]
pub static phys_base: AtomicUsize = AtomicUsize::new(0);
/// Address of the (virtual) `struct page` of frame 0, so `vmemmap + pfn` is the arena's memmap entry.
#[unsafe(no_mangle)]
pub static vmemmap_base: AtomicUsize = AtomicUsize::new(0);
/// The arena's virtual range, checked by the shadow `virt_to_page`; empty until the memmap exists.
#[unsafe(no_mangle)]
pub static lx_arena_start: AtomicUsize = AtomicUsize::new(0);
#[unsafe(no_mangle)]
pub static lx_arena_size: AtomicUsize = AtomicUsize::new(0);

/// A region the kernel owns for the rest of the program.
#[derive(Debug, Clone, Copy)]
pub struct Arena {
    base: NonNull<u8>,
    bus: BusAddr,
    size: usize,
}

#[derive(Debug, Snafu, PartialEq, Eq)]
pub enum ArenaError {
    #[snafu(display("arena base {base:#x}, bus address {bus:#x} or size {size:#x} is not page aligned"))]
    Unaligned { base: usize, bus: u64, size: usize },
    #[snafu(display("arena base {base:#x} and bus address {bus:#x} differ in their low {ARENA_ALIGN:#x} bits"))]
    Incongruent { base: usize, bus: u64 },
    #[snafu(display("an arena of {size:#x} bytes is too small to hold its memmap and a block"))]
    TooSmall { size: usize },
    #[snafu(display("an arena of {size:#x} bytes at bus address {bus:#x} wraps an address space"))]
    Wraps { bus: u64, size: usize },
}

impl Arena {
    /// # Safety
    /// `size` bytes from `base` must be readable and writable, unused by anything else for the rest of
    /// the program, and visible to devices at `bus` onwards.
    pub unsafe fn new(base: NonNull<u8>, bus: BusAddr, size: usize) -> Result<Self, ArenaError> {
        let (virt, bus_raw) = (base.as_ptr().addr(), bus.get());
        let page = PAGE_SIZE as u64;
        if !virt.is_multiple_of(PAGE_SIZE) || !bus_raw.is_multiple_of(page) || !size.is_multiple_of(PAGE_SIZE) {
            return Err(ArenaError::Unaligned { base: virt, bus: bus_raw, size });
        }
        if (virt as u64).wrapping_sub(bus_raw) % (ARENA_ALIGN as u64) != 0 {
            return Err(ArenaError::Incongruent { base: virt, bus: bus_raw });
        }
        if size < 2 * ARENA_ALIGN {
            return Err(ArenaError::TooSmall { size });
        }
        if virt.checked_add(size).is_none() || bus.checked_add(size as u64).is_none() {
            return Err(ArenaError::Wraps { bus: bus_raw, size });
        }
        Ok(Self { base, bus, size })
    }

    const fn base_pfn(&self) -> usize {
        (self.bus.get() >> PAGE_SHIFT) as usize
    }
}

struct State {
    arena: Arena,
    /// Set once the C has sized the memmap.
    buddy: Option<Buddy>,
}

struct Cell(UnsafeCell<Option<State>>);

// SAFETY: the C runs on one core without preemption, firmware notifies run no Linux code, and each
// call below finishes before another can start, so the state is never touched from two places at once.
unsafe impl Sync for Cell {}

static STATE: Cell = Cell(UnsafeCell::new(None));
static INSTALLED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Snafu)]
#[snafu(display("a kernel memory arena is already installed"))]
pub struct ArenaAlreadyInstalled;

/// Hand `arena` to the kernel, once per program.
pub fn install(arena: Arena) -> Result<(), ArenaAlreadyInstalled> {
    if INSTALLED.swap(true, Ordering::AcqRel) {
        return Err(ArenaAlreadyInstalled);
    }
    // SAFETY: the flag lets one caller in, and nothing reads the cell before the flag is set.
    unsafe { *STATE.0.get() = Some(State { arena, buddy: None }) };
    let offset = arena.base.as_ptr().addr().wrapping_sub(arena.bus.get() as usize);
    page_offset_base.store(offset, Ordering::Relaxed);
    phys_base.store(START_KERNEL_MAP.wrapping_sub(offset), Ordering::Relaxed);
    Ok(())
}

fn state(call: &CStr) -> &'static mut State {
    if !INSTALLED.load(Ordering::Acquire) {
        log::error!("kernel memory used before an arena was installed");
        trace_and_stop(call);
    }
    // SAFETY: installed, so the cell holds a state; see `Cell` for exclusivity.
    let Some(state) = (unsafe { &mut *STATE.0.get() }) else { trace_and_stop(call) };
    state
}

fn buddy(call: &CStr) -> (&'static mut Buddy, Arena) {
    let state = state(call);
    let arena = state.arena;
    let Some(buddy) = state.buddy.as_mut() else {
        log::error!("page freed or looked up before the memmap was set up");
        trace_and_stop(call)
    };
    (buddy, arena)
}

/// Pages in the arena that are free, or `None` before the memmap is set up.
pub fn free_page_count() -> Option<usize> {
    if !INSTALLED.load(Ordering::Acquire) {
        return None;
    }
    // SAFETY: installed; a shared read that ends before returning.
    unsafe { (*STATE.0.get()).as_ref() }.and_then(|state| state.buddy.as_ref()).map(Buddy::free_pages)
}

/// Pages at the front of an arena of `pages` taken by a memmap of `page_struct` bytes per page.
const fn memmap_pages(pages: usize, page_struct: usize) -> usize {
    (pages * page_struct).div_ceil(PAGE_SIZE)
}

/// Lay out the memmap for `page_struct_size`-byte page structs and start the page allocator; returns
/// the memmap. Called once by the C before its first allocation.
#[unsafe(no_mangle)]
pub extern "sysv64" fn lx_mem_setup(page_struct_size: c_ulong) -> *mut c_void {
    let state = state(c"lx_mem_setup");
    let arena = state.arena;
    let Ok(page_struct) = usize::try_from(page_struct_size) else { trace_and_stop(c"lx_mem_setup") };
    if state.buddy.is_some() {
        log::error!("the memmap is already set up");
        trace_and_stop(c"lx_mem_setup");
    }
    let pages = arena.size / PAGE_SIZE;
    let reserved = memmap_pages(pages, page_struct);
    let buddy = match Buddy::new(arena.base_pfn(), pages, reserved) {
        Ok(buddy) => buddy,
        Err(error) => {
            log::error!("{error}");
            trace_and_stop(c"lx_mem_setup");
        },
    };
    let memmap = arena.base.as_ptr();
    // SAFETY: the arena is ours and holds `reserved` pages at its front (`Arena::new` checked size).
    unsafe { memmap.write_bytes(0, reserved * PAGE_SIZE) };
    vmemmap_base.store(memmap.addr().wrapping_sub(arena.base_pfn().wrapping_mul(page_struct)), Ordering::Relaxed);
    lx_arena_start.store(memmap.addr(), Ordering::Relaxed);
    lx_arena_size.store(arena.size, Ordering::Relaxed);
    log::debug!(
        "kernel arena: {pages} pages at {memmap:p}, bus {:#x}; memmap {reserved} pages of {page_struct}-byte structs",
        arena.bus.get()
    );
    state.buddy = Some(buddy);
    memmap.cast()
}

fn virt_of(arena: Arena, index: usize) -> usize {
    arena.base.as_ptr().addr() + index * PAGE_SIZE
}

fn index_of(arena: Arena, virt: c_ulong, call: &CStr) -> usize {
    let offset = (virt as usize).wrapping_sub(arena.base.as_ptr().addr());
    if offset >= arena.size || !offset.is_multiple_of(PAGE_SIZE) {
        log::error!("{} of {virt:#x}: not a page in the kernel arena", call.to_string_lossy());
        trace_and_stop(call);
    }
    offset / PAGE_SIZE
}

fn order(order: c_uint) -> Option<u8> {
    u8::try_from(order).ok().filter(|&order| order <= MAX_ORDER)
}

/// A block of `2^order` pages, as its virtual address; 0 when none is left or the order is too large.
#[unsafe(no_mangle)]
pub extern "sysv64" fn lx_page_alloc(order_: c_uint) -> c_ulong {
    let (buddy, arena) = buddy(c"lx_page_alloc");
    order(order_).and_then(|order| buddy.alloc(order)).map_or(0, |index| virt_of(arena, index) as c_ulong)
}

/// Return the block of `2^order` pages at `virt`. A double free or a wrong order stops the program.
#[unsafe(no_mangle)]
pub extern "sysv64" fn lx_page_free(virt: c_ulong, order_: c_uint) {
    let (buddy, arena) = buddy(c"lx_page_free");
    let index = index_of(arena, virt, c"lx_page_free");
    let Some(order) = order(order_) else { trace_and_stop(c"lx_page_free") };
    if let Err(error) = buddy.free(index, order) {
        log::error!("freeing order-{order} pages at {virt:#x}: {error}");
        trace_and_stop(c"lx_page_free");
    }
}

/// Make the allocated block of `2^order` pages at `virt` single pages, each freed on its own.
#[unsafe(no_mangle)]
pub extern "sysv64" fn lx_page_split(virt: c_ulong, order_: c_uint) {
    let (buddy, arena) = buddy(c"lx_page_split");
    let index = index_of(arena, virt, c"lx_page_split");
    let Some(order) = order(order_) else { trace_and_stop(c"lx_page_split") };
    if let Err(error) = buddy.split(index, order) {
        log::error!("splitting order-{order} pages at {virt:#x}: {error}");
        trace_and_stop(c"lx_page_split");
    }
}

/// Whether the page at `virt` is free in the buddy allocator.
#[unsafe(no_mangle)]
pub extern "sysv64" fn lx_page_is_free(virt: c_ulong) -> bool {
    let (buddy, arena) = buddy(c"lx_page_is_free");
    buddy.is_free(index_of(arena, virt, c"lx_page_is_free"))
}

/// `virt_addr_valid`: whether `addr` lies in the arena, so `virt_to_page` may be applied to it.
#[unsafe(no_mangle)]
pub extern "sysv64" fn __virt_addr_valid(addr: c_ulong) -> bool {
    (addr as usize).wrapping_sub(lx_arena_start.load(Ordering::Relaxed)) < lx_arena_size.load(Ordering::Relaxed)
}

/// A memory-model violation the C found (`virt_to_page` outside the arena, a bad `kfree`): log `what`
/// and the address, then stop.
///
/// # Safety
/// `what` must be a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "sysv64" fn lx_mem_fault(what: *const c_char, addr: c_ulong) -> ! {
    // SAFETY: the caller passes a NUL-terminated string.
    let what = unsafe { CStr::from_ptr(what) };
    log::error!("{}: {addr:#x}", what.to_string_lossy());
    trace_and_stop(what)
}

#[cfg(test)]
mod tests {
    use assert2::check;
    use rstest::rstest;

    use super::*;

    fn arena(base: usize, bus: u64, size: usize) -> Result<Arena, ArenaError> {
        let base = NonNull::new(core::ptr::without_provenance_mut(base)).unwrap();
        // SAFETY: never installed or touched; only the checks run.
        unsafe { Arena::new(base, BusAddr::new(bus), size) }
    }

    #[rstest]
    #[case(0x7f00_0040_0000, 0x4000_0000, 1 << 28, None)]
    #[case(0x7f00_0040_0010, 0x4000_0000, 1 << 28, Some("Unaligned"))]
    #[case(0x7f00_0040_1000, 0x4000_0000, 1 << 28, Some("Incongruent"))]
    #[case(0x7f00_0040_0000, 0x4000_0000, ARENA_ALIGN, Some("TooSmall"))]
    #[case(0x7f00_0040_0000, 0xffff_ffff_ffc0_0000, 1 << 28, Some("Wraps"))]
    fn arena_checks(#[case] base: usize, #[case] bus: u64, #[case] size: usize, #[case] error: Option<&str>) {
        let got = arena(base, bus, size).err().map(|e| alloc::format!("{e:?}"));
        check!(got.as_deref().map(|e| e.split(' ').next().unwrap_or("")) == error);
    }

    #[test]
    fn memmap_covers_every_page() {
        check!(memmap_pages(65_536, 64) == 1024);
        check!(memmap_pages(65_537, 64) == 1025);
    }
}
