//! The kernel data the shim has not implemented, made to fault instead of reading zero.
//!
//! Its storage sits on pages of its own, which are mapped no-access before any initcall runs. A touch raises
//! SIGSEGV, and the handler names the variable and where the C was, then ends the process like a stub hit.

use std::backtrace::Backtrace;
use std::io::Write as _;
use std::ops::Range;

use ferrite_wifi_sys::{unimplemented_data, unimplemented_regions};

/// Exit status of a run that touched unimplemented data, beside the halt hook's 86.
pub const EXIT_DATA: u8 = 87;
/// What a shell reports for a process killed by SIGSEGV, kept for faults elsewhere.
const EXIT_SEGV: u8 = 139;
const PAGE: usize = 4096;

fn fenced(address: usize) -> bool {
    unimplemented_regions().iter().any(|region| region.contains(&address))
}

/// Map the regions no-access and install the SIGSEGV handler that reports a touch.
pub fn install() -> Result<(), String> {
    let regions = unimplemented_regions();
    if let Some(region) = regions.iter().find(|r| r.start % PAGE != 0 || r.end % PAGE != 0) {
        return Err(format!("unimplemented data region {region:x?} is not page aligned"));
    }
    if let Some(stray) = unimplemented_data().iter().find(|d| !regions.iter().any(|r| covers(r, &d.range()))) {
        return Err(format!("{} lies outside the unimplemented data regions", stray.name().to_string_lossy()));
    }
    // SAFETY: an all-zero sigaction is a valid starting value; the fields that matter are set below.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = on_fault as *const () as usize;
    action.sa_flags = libc::SA_SIGINFO;
    // SAFETY: `action` is initialized and the handler has the SA_SIGINFO signature.
    if unsafe { libc::sigaction(libc::SIGSEGV, &raw const action, std::ptr::null_mut()) } != 0 {
        return Err(format!("installing the SIGSEGV handler failed: {}", std::io::Error::last_os_error()));
    }
    for region in regions.iter().filter(|r| !r.is_empty()) {
        // SAFETY: the range is whole pages that hold only the generated storage, which nothing else uses.
        if unsafe { libc::mprotect(region.start as *mut libc::c_void, region.len(), libc::PROT_NONE) } != 0 {
            return Err(format!("mprotect of {region:x?} failed: {}", std::io::Error::last_os_error()));
        }
    }
    Ok(())
}

fn covers(outer: &Range<usize>, inner: &Range<usize>) -> bool {
    outer.start <= inner.start && inner.end <= outer.end
}

/// Report a fault in the regions by variable name; any other fault is a crash and is reported as one.
/// Formatting and the backtrace are not async-signal-safe, which a handler that never returns accepts.
extern "C" fn on_fault(_: libc::c_int, info: *mut libc::siginfo_t, _: *mut libc::c_void) {
    // SAFETY: the kernel passes a valid siginfo for a SA_SIGINFO handler.
    let address = unsafe { (*info).si_addr() } as usize;
    let named = unimplemented_data().iter().find(|d| d.range().contains(&address));
    let report = match named {
        Some(data) => {
            format!("touched unimplemented data {}+{:#x}", data.name().to_string_lossy(), address - data.range().start)
        },
        None if fenced(address) => format!("touched unimplemented data at {address:#x}"),
        None => format!("segmentation fault at {address:#x}"),
    };
    let status = if fenced(address) { EXIT_DATA } else { EXIT_SEGV };
    let backtrace = Backtrace::force_capture();
    // The process ends right after; a failed write has nowhere to be reported.
    let _ = writeln!(std::io::stderr().lock(), "{report}\n{backtrace}");
    std::process::exit(i32::from(status))
}
