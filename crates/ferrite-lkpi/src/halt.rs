//! What happens when the C reaches a kernel symbol nobody implements.

use alloc::borrow::Cow;
use core::ffi::{CStr, c_char};
use core::sync::atomic::{AtomicUsize, Ordering};

use snafu::Snafu;

/// How the host program stops once a stub has been hit; it must not return.
pub type HaltHook = fn(&CStr) -> !;

/// Zero while unset, otherwise a `HaltHook` as an address.
static HALT_HOOK: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Snafu)]
#[snafu(display("a halt hook is already registered"))]
pub struct HookAlreadySet;

/// Register the program's halt hook. It can be set once.
pub fn set_halt_hook(hook: HaltHook) -> Result<(), HookAlreadySet> {
    HALT_HOOK
        .compare_exchange(0, hook as usize, Ordering::AcqRel, Ordering::Acquire)
        .map(drop)
        .map_err(|_| HookAlreadySet)
}

fn halt_hook() -> Option<HaltHook> {
    let raw = HALT_HOOK.load(Ordering::Acquire);
    // SAFETY: a nonzero value was stored by `set_halt_hook` from a `HaltHook`, and addresses and function pointers have the same size.
    (raw != 0).then(|| unsafe { core::mem::transmute::<usize, HaltHook>(raw) })
}

/// Log that the stub `name` was hit, then stop in the registered hook; with none set, spin.
#[expect(clippy::option_if_let_else, reason = "the arms return the never type, which closures cannot name")]
pub fn trace_and_stop(name: &CStr) -> ! {
    let text: Cow<'_, str> = name.to_string_lossy();
    log::error!("lkpi stub hit: {text}");
    match halt_hook() {
        Some(hook) => hook(name),
        None => loop {
            core::hint::spin_loop();
        },
    }
}

/// What the C stubs for variadic kernel functions call.
///
/// # Safety
/// `name` must point to a NUL-terminated string that outlives the call.
#[unsafe(no_mangle)]
pub unsafe extern "sysv64" fn lx_stub_hit(name: *const c_char) -> ! {
    // SAFETY: the caller guarantees a valid NUL-terminated string.
    trace_and_stop(unsafe { CStr::from_ptr(name) })
}
