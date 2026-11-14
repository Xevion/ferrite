//! The Linux Wi-Fi C (mac80211, cfg80211, rtw89, iwlwifi, mt76), compiled by `build.rs` from the snapshot that
//! `ferrite-dev vendor` makes, the tables of its initcalls and exitcalls, and the region holding the kernel
//! data the shim has not implemented yet.
//!
//! The C imports the kernel API from `ferrite-lkpi`, which a program linking this crate must also link.

#![no_std]

use core::ffi::{CStr, c_char, c_int, c_ulong, c_void};
use core::ops::Range;

/// One row of the C `lx_initcalls` table.
#[repr(C)]
pub struct Initcall {
    level: *const c_char,
    name: *const c_char,
    slot: *const unsafe extern "sysv64" fn() -> c_int,
}

/// One row of the C `lx_exitcalls` table.
#[repr(C)]
pub struct Exitcall {
    name: *const c_char,
    slot: *const unsafe extern "sysv64" fn(),
}

unsafe extern "C" {
    static lx_initcalls: [Initcall; 0];
    static lx_initcall_count: c_ulong;
    static lx_exitcalls: [Exitcall; 0];
    static lx_exitcall_count: c_ulong;
    static lx_unimplemented_begin: [u8; 0];
    static lx_unimplemented_end: [u8; 0];
    static lx_unimplemented_ro_begin: [u8; 0];
    static lx_unimplemented_ro_end: [u8; 0];
    static lx_unimplemented_data: [DataSymbol; 0];
    static lx_unimplemented_data_count: c_ulong;
}

/// One row of the C `lx_unimplemented_data` table: a kernel variable the shim has not implemented.
#[repr(C)]
pub struct DataSymbol {
    name: *const c_char,
    address: *const c_void,
    size: c_ulong,
}

impl DataSymbol {
    #[must_use]
    pub fn name(&self) -> &'static CStr {
        // SAFETY: the generated table points at string literals.
        unsafe { CStr::from_ptr(self.name) }
    }

    /// Where the variable's storage sits in this process.
    #[must_use]
    pub fn range(&self) -> Range<usize> {
        let start = self.address as usize;
        start..start + self.size as usize
    }
}

/// The page-aligned regions, writable and read-only, holding the storage of every unimplemented variable, so
/// a host can make them fault.
#[must_use]
pub fn unimplemented_regions() -> [Range<usize>; 2] {
    [
        (&raw const lx_unimplemented_begin) as usize..(&raw const lx_unimplemented_end) as usize,
        (&raw const lx_unimplemented_ro_begin) as usize..(&raw const lx_unimplemented_ro_end) as usize,
    ]
}

/// The unimplemented variables, sorted by name.
#[must_use]
pub fn unimplemented_data() -> &'static [DataSymbol] {
    // SAFETY: the generated table holds `lx_unimplemented_data_count` rows and is never written.
    unsafe { core::slice::from_raw_parts(lx_unimplemented_data.as_ptr(), lx_unimplemented_data_count as usize) }
}

/// The initcalls in the order the kernel runs them: by level, then link order.
#[must_use]
pub fn initcalls() -> &'static [Initcall] {
    // SAFETY: the generated table holds `lx_initcall_count` rows and is never written.
    unsafe { core::slice::from_raw_parts(lx_initcalls.as_ptr(), lx_initcall_count as usize) }
}

/// The exitcalls in the order that tears modules down: the reverse of their setup.
#[must_use]
pub fn exitcalls() -> &'static [Exitcall] {
    // SAFETY: the generated table holds `lx_exitcall_count` rows and is never written.
    unsafe { core::slice::from_raw_parts(lx_exitcalls.as_ptr(), lx_exitcall_count as usize) }
}

impl Initcall {
    /// The initcall level (`"4"` for `subsys_initcall`).
    #[must_use]
    pub fn level(&self) -> &'static CStr {
        // SAFETY: the generated table points at string literals.
        unsafe { CStr::from_ptr(self.level) }
    }

    /// The name of the function the C registered.
    #[must_use]
    pub fn name(&self) -> &'static CStr {
        // SAFETY: the generated table points at string literals.
        unsafe { CStr::from_ptr(self.name) }
    }

    /// Run the initcall; the result is a kernel return code, zero or a negated errno.
    ///
    /// # Safety
    /// Runs kernel C that expects what the shim provides, and no initcall may be running already.
    #[must_use]
    pub unsafe fn call(&self) -> c_int {
        // SAFETY: the slot holds the initcall function itself, set at link time.
        unsafe { (*self.slot)() }
    }
}

impl Exitcall {
    /// The name of the function the C registered.
    #[must_use]
    pub fn name(&self) -> &'static CStr {
        // SAFETY: the generated table points at string literals.
        unsafe { CStr::from_ptr(self.name) }
    }

    /// Run the exitcall.
    ///
    /// # Safety
    /// Runs kernel C that expects what the shim provides, and its initcall must have succeeded.
    pub unsafe fn call(&self) {
        // SAFETY: the slot holds the exitcall function itself, set at link time.
        unsafe { (*self.slot)() }
    }
}
