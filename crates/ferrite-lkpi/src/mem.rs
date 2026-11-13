//! `kmalloc` and `kfree` over the program's global allocator, which task creation needs.
//!
//! Every block is aligned to its size rounded up to a power of two (16 to 4096 bytes), as Linux
//! guarantees for kmalloc, and carries its layout in the 16 bytes below the pointer. Caches are ignored:
//! the slab layer proper is not implemented.

use alloc::alloc::{Layout, alloc, alloc_zeroed, dealloc};
use core::ffi::{c_int, c_uint, c_ulong, c_void};

/// `__GFP_ZERO`.
const GFP_ZERO: c_uint = 1 << 8;
const MIN_ALIGN: usize = 16;
const MAX_ALIGN: usize = 4096;
const HEADER: usize = 2 * size_of::<usize>();

fn layout(size: usize) -> Option<(Layout, usize)> {
    let align = size.checked_next_power_of_two()?.clamp(MIN_ALIGN, MAX_ALIGN);
    let layout = Layout::from_size_align(size.checked_add(align)?, align).ok()?;
    Some((layout, align))
}

#[expect(clippy::cast_ptr_alignment, reason = "the header sits below a block aligned to at least 16 bytes")]
fn kmalloc(size: usize, flags: c_uint) -> *mut c_void {
    let Some((layout, align)) = layout(size) else { return core::ptr::null_mut() };
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
    if (block as usize) <= MIN_ALIGN {
        return;
    }
    let block = block.cast::<u8>().cast_mut();
    // SAFETY: the caller guarantees a block from `kmalloc`, which wrote the header below it.
    let [size, align] = unsafe { block.sub(HEADER).cast::<[usize; 2]>().read() };
    // SAFETY: the layout is the one `kmalloc` allocated with, rebuilt from the header.
    unsafe { dealloc(block.sub(align), Layout::from_size_align_unchecked(size + align, align)) };
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
