//! Where the shadow `printk` hands a formatted kernel message: the `log` facade.

use alloc::string::String;
use core::ffi::{c_char, c_int};

use log::Level;

/// The `log` level of a kernel level: 0 to 3 are errors, 4 a warning, and the rest routine detail.
const fn level(kernel: c_int) -> Level {
    match kernel {
        ..=3 => Level::Error,
        4 => Level::Warn,
        5 => Level::Info,
        6 => Level::Debug,
        _ => Level::Trace,
    }
}

/// Log the `len` bytes at `msg`, minus a trailing newline, at the `log` level of kernel level `level`.
///
/// # Safety
/// `msg` must be valid for `len` bytes of reads.
#[unsafe(no_mangle)]
pub unsafe extern "sysv64" fn lx_printk(kernel_level: c_int, msg: *const c_char, len: usize) {
    // SAFETY: the caller guarantees `len` readable bytes at `msg`.
    let bytes = unsafe { core::slice::from_raw_parts(msg.cast::<u8>(), len) };
    let text: String = String::from_utf8_lossy(bytes).into_owned();
    log::log!(target: "kernel", level(kernel_level), "{}", text.trim_end_matches('\n'));
}
