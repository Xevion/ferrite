//! `kmalloc`, `kvmalloc`, `vmalloc` and whole pages over the program's global allocator.
//!
//! Every block is aligned to its size rounded up to a power of two (16 to 4096 bytes), as Linux
//! guarantees for kmalloc, or more when asked, and carries its size and alignment in the 16 bytes below
//! the pointer. Caches are ignored: the slab layer proper is not implemented. There is no virtual
//! mapping, so a vmalloc block is a kmalloc block and no address is a vmalloc address.

use alloc::alloc::{Layout, alloc, alloc_zeroed, dealloc};
use core::ffi::{c_int, c_uint, c_ulong, c_void};

/// `__GFP_ZERO`.
const GFP_ZERO: c_uint = 1 << 8;
const MIN_ALIGN: usize = 16;
const MAX_ALIGN: usize = 4096;
const HEADER: usize = 2 * size_of::<usize>();

const PAGE_SIZE: usize = 4096;
/// `ZERO_SIZE_PTR`: what a zero-byte request returns, and what free ignores with null.
const ZERO_SIZE_PTR: usize = 16;

fn layout(size: usize, min_align: usize) -> Option<(Layout, usize)> {
    let align =
        size.checked_next_power_of_two()?.clamp(MIN_ALIGN, MAX_ALIGN).max(min_align.checked_next_power_of_two()?);
    let layout = Layout::from_size_align(size.checked_add(align)?, align).ok()?;
    Some((layout, align))
}

fn kmalloc(size: usize, flags: c_uint) -> *mut c_void {
    kmalloc_aligned(size, 0, flags)
}

#[expect(clippy::cast_ptr_alignment, reason = "the header sits below a block aligned to at least 16 bytes")]
fn kmalloc_aligned(size: usize, min_align: usize, flags: c_uint) -> *mut c_void {
    let Some((layout, align)) = layout(size, min_align) else { return core::ptr::null_mut() };
    // SAFETY: the layout is nonzero in size.
    let base = unsafe { if flags & GFP_ZERO != 0 { alloc_zeroed(layout) } else { alloc(layout) } };
    if base.is_null() {
        log::warn!("kmalloc of {size} bytes failed");
        return base.cast();
    }
    // SAFETY: `align` is at least `HEADER`, so the block and its header lie inside the allocation.
    unsafe {
        let block = base.add(align);
        block.sub(HEADER).cast::<[usize; 2]>().write([size, align]);
        block.cast()
    }
}

#[unsafe(no_mangle)]
pub extern "sysv64" fn __kmalloc_noprof(size: usize, flags: c_uint) -> *mut c_void {
    kmalloc(size, flags)
}

#[unsafe(no_mangle)]
pub extern "sysv64" fn __kmalloc_large_noprof(size: usize, flags: c_uint) -> *mut c_void {
    kmalloc(size, flags)
}

#[unsafe(no_mangle)]
pub extern "sysv64" fn __kmalloc_node_noprof(size: usize, flags: c_uint, _node: c_int) -> *mut c_void {
    kmalloc(size, flags)
}

#[unsafe(no_mangle)]
pub extern "sysv64" fn __kmalloc_node_track_caller_noprof(
    size: usize,
    flags: c_uint,
    _node: c_int,
    _caller: c_ulong,
) -> *mut c_void {
    kmalloc(size, flags)
}

#[unsafe(no_mangle)]
pub extern "sysv64" fn __kmalloc_cache_noprof(_cache: *mut c_void, flags: c_uint, size: usize) -> *mut c_void {
    kmalloc(size, flags)
}

#[unsafe(no_mangle)]
pub extern "sysv64" fn __kmalloc_cache_node_noprof(
    _cache: *mut c_void,
    flags: c_uint,
    _node: c_int,
    size: usize,
) -> *mut c_void {
    kmalloc(size, flags)
}

/// Free a block from the functions above; null and `ZERO_SIZE_PTR` (16) are ignored.
///
/// # Safety
/// `block` must be null, `ZERO_SIZE_PTR`, or a live block from this module's kmalloc.
#[unsafe(no_mangle)]
#[expect(clippy::cast_ptr_alignment, reason = "the header sits below a block aligned to at least 16 bytes")]
pub unsafe extern "sysv64" fn kfree(block: *const c_void) {
    if (block as usize) <= ZERO_SIZE_PTR {
        return;
    }
    let block = block.cast::<u8>().cast_mut();
    // SAFETY: the caller guarantees a block from `kmalloc`, which wrote the header below it.
    let [size, align] = unsafe { block.sub(HEADER).cast::<[usize; 2]>().read() };
    // SAFETY: the layout is the one `kmalloc` allocated with, rebuilt from the header.
    unsafe { dealloc(block.sub(align), Layout::from_size_align_unchecked(size + align, align)) };
}

/// The size and alignment `kmalloc_aligned` wrote below `block`, a live block from it.
#[expect(clippy::cast_ptr_alignment, reason = "the header sits below a block aligned to at least 16 bytes")]
unsafe fn header(block: *const c_void) -> [usize; 2] {
    // SAFETY: the caller guarantees a block from `kmalloc_aligned`, which wrote the header below it.
    unsafe { block.cast::<u8>().sub(HEADER).cast::<[usize; 2]>().read() }
}

/// A block of `size` bytes with the contents of `old` up to the smaller size, then `old` freed.
///
/// # Safety
/// `old` must be null, `ZERO_SIZE_PTR`, or a live block from this module.
unsafe fn realloc(old: *const c_void, size: usize, align: usize, flags: c_uint) -> *mut c_void {
    let live = (old as usize) > ZERO_SIZE_PTR;
    if size == 0 {
        // SAFETY: as the caller guarantees.
        unsafe { kfree(old) };
        return core::ptr::with_exposed_provenance_mut(ZERO_SIZE_PTR);
    }
    let block = kmalloc_aligned(size, align, flags);
    if live && !block.is_null() {
        // SAFETY: `old` is live, so its header holds its size; both blocks hold the bytes copied.
        unsafe {
            let [old_size, _] = header(old);
            core::ptr::copy_nonoverlapping(old.cast::<u8>(), block.cast::<u8>(), old_size.min(size));
            kfree(old);
        }
    }
    block
}

#[unsafe(no_mangle)]
pub extern "sysv64" fn __kvmalloc_node_noprof(size: usize, align: c_ulong, flags: c_uint, _node: c_int) -> *mut c_void {
    kmalloc_aligned(size, usize::try_from(align).unwrap_or(MAX_ALIGN), flags)
}

/// # Safety
/// As [`kfree`].
#[unsafe(no_mangle)]
pub unsafe extern "sysv64" fn kvfree(block: *const c_void) {
    // SAFETY: as the caller guarantees.
    unsafe { kfree(block) };
}

/// # Safety
/// As [`kfree`].
#[unsafe(no_mangle)]
pub unsafe extern "sysv64" fn vfree(block: *const c_void) {
    // SAFETY: as the caller guarantees.
    unsafe { kfree(block) };
}

/// Zero a block, then free it.
///
/// # Safety
/// As [`kfree`].
#[unsafe(no_mangle)]
pub unsafe extern "sysv64" fn kfree_sensitive(block: *const c_void) {
    if (block as usize) <= ZERO_SIZE_PTR {
        return;
    }
    // SAFETY: a live block, so its header holds its size and that many bytes are writable.
    unsafe {
        let [size, _] = header(block);
        core::ptr::write_bytes(block.cast::<u8>().cast_mut(), 0, size);
        kfree(block);
    }
}

/// The usable size of a block: exactly what was asked for.
///
/// # Safety
/// `block` must be null, `ZERO_SIZE_PTR`, or a live block from this module.
#[unsafe(no_mangle)]
pub unsafe extern "sysv64" fn ksize(block: *const c_void) -> usize {
    if (block as usize) <= ZERO_SIZE_PTR {
        return 0;
    }
    // SAFETY: as the caller guarantees.
    unsafe { header(block)[0] }
}

/// Blocks are never larger than asked, so a request rounds up to itself.
#[unsafe(no_mangle)]
pub extern "sysv64" fn kmalloc_size_roundup(size: usize) -> usize {
    size
}

/// # Safety
/// As [`kfree`] for `old`.
#[unsafe(no_mangle)]
pub unsafe extern "sysv64" fn krealloc_node_align_noprof(
    old: *const c_void,
    size: usize,
    align: c_ulong,
    flags: c_uint,
    _node: c_int,
) -> *mut c_void {
    // SAFETY: as the caller guarantees.
    unsafe { realloc(old, size, usize::try_from(align).unwrap_or(MAX_ALIGN), flags) }
}

/// # Safety
/// As [`kfree`] for `old`.
#[unsafe(no_mangle)]
pub unsafe extern "sysv64" fn kvrealloc_node_align_noprof(
    old: *const c_void,
    size: usize,
    align: c_ulong,
    flags: c_uint,
    _node: c_int,
) -> *mut c_void {
    // SAFETY: as the caller guarantees.
    unsafe { realloc(old, size, usize::try_from(align).unwrap_or(MAX_ALIGN), flags) }
}

#[unsafe(no_mangle)]
pub extern "sysv64" fn __vmalloc_noprof(size: c_ulong, flags: c_uint) -> *mut c_void {
    usize::try_from(size).map_or(core::ptr::null_mut(), |size| kmalloc(size, flags))
}

#[unsafe(no_mangle)]
pub extern "sysv64" fn vzalloc_noprof(size: c_ulong) -> *mut c_void {
    __vmalloc_noprof(size, GFP_ZERO)
}

#[unsafe(no_mangle)]
pub extern "sysv64" fn is_vmalloc_addr(_addr: *const c_void) -> bool {
    false
}

#[unsafe(no_mangle)]
pub extern "sysv64" fn is_vmalloc_or_module_addr(_addr: *const c_void) -> c_int {
    0
}

/// `2^order` whole pages, page aligned, as an address; 0 on failure.
#[unsafe(no_mangle)]
pub extern "sysv64" fn get_free_pages_noprof(flags: c_uint, order: c_uint) -> c_ulong {
    let Some(size) = PAGE_SIZE.checked_shl(order) else { return 0 };
    kmalloc_aligned(size, PAGE_SIZE, flags).expose_provenance() as c_ulong
}

/// # Safety
/// `addr` must be 0 or an address `get_free_pages_noprof` returned and not yet freed.
#[unsafe(no_mangle)]
pub unsafe extern "sysv64" fn free_pages(addr: c_ulong, _order: c_uint) {
    // SAFETY: as the caller guarantees; the block came from `kmalloc_aligned`.
    unsafe { kfree(core::ptr::with_exposed_provenance(addr as usize)) };
}

#[cfg(test)]
mod tests {
    use assert2::check;
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case(1, 16)]
    #[case(24, 32)]
    #[case(64, 64)]
    #[case(3000, 4096)]
    #[case(100_000, 4096)]
    fn blocks_are_aligned_to_their_rounded_size_and_zeroed_on_request(#[case] size: usize, #[case] align: usize) {
        let block = __kmalloc_noprof(size, GFP_ZERO);
        check!(block as usize % align == 0);
        // SAFETY: just allocated with `size` bytes.
        let bytes = unsafe { core::slice::from_raw_parts(block.cast::<u8>(), size) };
        check!(bytes.iter().all(|&b| b == 0));
        // SAFETY: a live block from kmalloc.
        unsafe { kfree(block) };
    }
}
