//! `memcpy`, `memmove`, `memset`, `memcmp`, `bcmp` and `strlen`, which Rust calls by name; the Linux C
//! calls the first three (its own string functions are renamed apart).
//!
//! `no_builtins` on the crate stops LLVM from turning these bodies back into calls to the functions they
//! define.

#[cfg(test)]
mod tests;

use core::arch::asm;
use core::ffi::c_int;
#[cfg(all(ferrite_app, target_os = "linux"))]
use core::ffi::{c_char, c_void};

/// Copy `n` bytes from the lowest address up.
///
/// # Safety
/// `src` is readable and `dst` writable for `n` bytes; an overlap is allowed only below `src`.
unsafe fn copy_up(dst: *mut u8, src: *const u8, n: usize) {
    // SAFETY: the caller's ranges; rep movsb leaves the direction flag as found, clear.
    unsafe {
        asm!("rep movsb", inout("rcx") n => _, inout("rdi") dst => _, inout("rsi") src => _,
            options(nostack, preserves_flags));
    }
}

/// Copy `n` bytes from the highest address down, so a destination above an overlapping source is safe.
///
/// # Safety
/// `src` is readable and `dst` writable for `n` bytes.
unsafe fn copy_down(dst: *mut u8, src: *const u8, n: usize) {
    if n == 0 {
        return;
    }
    // SAFETY: the caller's ranges, walked from their last bytes; the direction flag is cleared again before
    // the block ends, as the ABI requires.
    unsafe {
        asm!("std", "rep movsb", "cld", inout("rcx") n => _, inout("rdi") dst.add(n - 1) => _,
            inout("rsi") src.add(n - 1) => _, options(nostack));
    }
}

/// Copy `n` bytes between ranges that may overlap.
///
/// # Safety
/// `src` is readable and `dst` writable for `n` bytes.
unsafe fn move_bytes(dst: *mut u8, src: *const u8, n: usize) {
    // A destination starting inside the source would overwrite bytes before they are read going up.
    let dst_inside_src = (dst as usize).wrapping_sub(src as usize) < n;
    // SAFETY: the caller's ranges; the direction is chosen so every byte is read before it is written.
    unsafe {
        if dst_inside_src && dst.cast_const() != src {
            copy_down(dst, src, n);
        } else {
            copy_up(dst, src, n);
        }
    }
}

/// Fill `n` bytes with `byte`.
///
/// # Safety
/// `dst` is writable for `n` bytes.
unsafe fn fill(dst: *mut u8, byte: u8, n: usize) {
    // SAFETY: the caller's range.
    unsafe {
        asm!("rep stosb", inout("rcx") n => _, inout("rdi") dst => _, in("al") byte,
            options(nostack, preserves_flags));
    }
}

/// The difference of the first differing bytes, compared as unsigned, or 0 when all `n` match.
///
/// # Safety
/// `lhs` and `rhs` are readable for `n` bytes.
unsafe fn compare(lhs: *const u8, rhs: *const u8, n: usize) -> c_int {
    for i in 0..n {
        // SAFETY: `i < n`, inside both of the caller's ranges.
        let (l, r) = unsafe { (*lhs.add(i), *rhs.add(i)) };
        if l != r {
            return c_int::from(l) - c_int::from(r);
        }
    }
    0
}

/// Bytes before the first NUL.
///
/// # Safety
/// `s` is readable up to and including a NUL byte.
unsafe fn length(s: *const u8) -> usize {
    let mut n = 0;
    // SAFETY: every byte up to the NUL is readable.
    while unsafe { *s.add(n) } != 0 {
        n += 1;
    }
    n
}

/// C `memcpy`.
///
/// # Safety
/// As C: `src` readable and `dest` writable for `n` bytes, not overlapping.
#[cfg(all(ferrite_app, target_os = "linux"))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcpy(dest: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
    // SAFETY: the C contract above.
    unsafe { copy_up(dest.cast(), src.cast(), n) };
    dest
}

/// C `memmove`.
///
/// # Safety
/// As C: `src` readable and `dest` writable for `n` bytes; they may overlap.
#[cfg(all(ferrite_app, target_os = "linux"))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memmove(dest: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
    // SAFETY: the C contract above.
    unsafe { move_bytes(dest.cast(), src.cast(), n) };
    dest
}

/// C `memset`: only the low byte of `c` is stored.
///
/// # Safety
/// As C: `dest` writable for `n` bytes.
#[cfg(all(ferrite_app, target_os = "linux"))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memset(dest: *mut c_void, c: c_int, n: usize) -> *mut c_void {
    // SAFETY: the C contract above.
    unsafe { fill(dest.cast(), c.to_le_bytes()[0], n) };
    dest
}

/// C `memcmp`.
///
/// # Safety
/// As C: `s1` and `s2` readable for `n` bytes.
#[cfg(all(ferrite_app, target_os = "linux"))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcmp(s1: *const c_void, s2: *const c_void, n: usize) -> c_int {
    // SAFETY: the C contract above.
    unsafe { compare(s1.cast(), s2.cast(), n) }
}

/// `bcmp`: zero exactly when the ranges are equal.
///
/// # Safety
/// As C: `s1` and `s2` readable for `n` bytes.
#[cfg(all(ferrite_app, target_os = "linux"))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bcmp(s1: *const c_void, s2: *const c_void, n: usize) -> c_int {
    // SAFETY: the C contract above.
    unsafe { compare(s1.cast(), s2.cast(), n) }
}

/// C `strlen`, which `core::ffi::CStr::from_ptr` calls on this target.
///
/// # Safety
/// As C: `s` points to a NUL-terminated string.
#[cfg(all(ferrite_app, target_os = "linux"))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn strlen(s: *const c_char) -> usize {
    // SAFETY: the C contract above.
    unsafe { length(s.cast()) }
}
