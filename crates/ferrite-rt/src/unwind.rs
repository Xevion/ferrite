//! Unwinder entry points the precompiled `alloc` references from its cleanup paths.
//!
//! With `panic=abort` nothing ever starts an unwind, so neither can be reached; each traps if it is.

use core::arch::asm;
use core::ffi::c_void;

/// Resumes an unwind in progress, which never exists here.
///
/// # Safety
/// Never called: it traps.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn _Unwind_Resume(_exception: *mut c_void) -> ! {
    // SAFETY: ud2 raises an invalid-opcode fault and touches no state.
    unsafe { asm!("ud2", options(noreturn, nomem, nostack)) }
}

/// The personality routine the unwinder would call for Rust frames, which it never runs here.
///
/// # Safety
/// Never called: it traps.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_eh_personality() -> ! {
    // SAFETY: as above.
    unsafe { asm!("ud2", options(noreturn, nomem, nostack)) }
}
