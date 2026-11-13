//! What an app built for `x86_64-unknown-linux-gnu` needs from the libc it does not link.
//!
//! On that target `compiler_builtins` leaves the memory functions and `strlen` to libc, and the precompiled `alloc`
//! keeps references into the unwinder even under `panic=abort`. The symbols exist only in an app build
//! (`ferrite_app` on Linux): a host build keeps glibc's, and the UEFI target has its own.

#![no_std]
#![no_builtins]

#[cfg(test)]
extern crate std;

#[cfg(any(test, all(ferrite_app, target_os = "linux")))]
mod mem;
#[cfg(all(ferrite_app, target_os = "linux"))]
mod unwind;
