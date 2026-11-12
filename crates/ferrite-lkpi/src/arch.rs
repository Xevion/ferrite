//! What the shadow `asm/irqflags.h` and `asm/bug.h` headers call instead of `cli`, `sti` and `ud2`.
//!
//! The shim is uniprocessor and cooperative, so there is nothing to mask: the state word only has to
//! round-trip, with bit 0 set while "masked". The other bits belong to a platform that needs them.

use core::ffi::{CStr, c_char, c_int, c_ulong};
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::trace_and_stop;

const MASKED: usize = 1;

static IRQ_STATE: AtomicUsize = AtomicUsize::new(0);

/// Mask, and return the state to hand back to `lx_irq_restore`.
#[unsafe(no_mangle)]
pub extern "sysv64" fn lx_irq_save() -> c_ulong {
    IRQ_STATE.swap(MASKED, Ordering::AcqRel) as c_ulong
}

/// Return to a state `lx_irq_save` produced.
#[unsafe(no_mangle)]
pub extern "sysv64" fn lx_irq_restore(flags: c_ulong) {
    IRQ_STATE.store(flags as usize & MASKED, Ordering::Release);
}

#[unsafe(no_mangle)]
pub extern "sysv64" fn lx_irq_disable() {
    IRQ_STATE.store(MASKED, Ordering::Release);
}

#[unsafe(no_mangle)]
pub extern "sysv64" fn lx_irq_enable() {
    IRQ_STATE.store(0, Ordering::Release);
}

/// The current state, unchanged.
#[unsafe(no_mangle)]
pub extern "sysv64" fn lx_irq_flags() -> c_ulong {
    IRQ_STATE.load(Ordering::Acquire) as c_ulong
}

/// `file` as text for a log line; a null pointer reads as unknown.
///
/// # Safety
/// A non-null `file` must point to a NUL-terminated string that outlives the call.
unsafe fn source_file<'a>(file: *const c_char) -> alloc::borrow::Cow<'a, str> {
    if file.is_null() {
        return "?".into();
    }
    // SAFETY: non-null, and the caller guarantees a valid NUL-terminated string.
    unsafe { CStr::from_ptr(file) }.to_string_lossy()
}

/// `WARN()`: report where, then carry on.
///
/// # Safety
/// `file` must be null or point to a NUL-terminated string that outlives the call.
#[unsafe(no_mangle)]
pub unsafe extern "sysv64" fn lx_warn(file: *const c_char, line: c_int) {
    // SAFETY: forwarded from the caller's contract.
    let file = unsafe { source_file(file) };
    log::warn!("kernel WARN at {file}:{line}");
}

/// `BUG()`: report where, then stop as a stub hit does.
///
/// # Safety
/// `file` must be null or point to a NUL-terminated string that outlives the call.
#[unsafe(no_mangle)]
pub unsafe extern "sysv64" fn lx_bug(file: *const c_char, line: c_int) -> ! {
    // SAFETY: forwarded from the caller's contract.
    let file = unsafe { source_file(file) };
    log::error!("kernel BUG at {file}:{line}");
    trace_and_stop(c"lx_bug")
}
