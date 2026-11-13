//! The Linux kernel API the Wi-Fi drivers call, implemented for the platform they run on.
//!
//! Every function crossing into the C is `extern "sysv64"`: the C is built for bare `x86_64`, whatever
//! ABI the Rust target uses. Kernel symbols nothing implements yet are generated stubs that name
//! themselves and stop the program.

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

mod arch;
mod buddy;
pub mod firmware;
mod halt;
pub mod mem;
mod printk;
pub mod random;
pub mod sched;
#[cfg(feature = "stubs")]
mod stubs;
pub mod task;
pub mod time;

pub use halt::{HaltHook, HookAlreadySet, lx_stub_hit, set_halt_hook, trace_and_stop};
