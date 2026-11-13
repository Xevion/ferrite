//! Memory-test building blocks at every x86-64 SIMD tier.
//!
//! Each block (fill, verify, own-address fill and verify, PRSG fill and verify, a moving-inversion
//! step, a block copy) works on one [`Region`] chunk with one [`Kernel`], a [`Tier`] paired with a
//! [`Stores`] kind. The caller walks the memory under test in chunks, so it can interleave other
//! work, and picks the kernels from the [`Caps`] it built from CPUID and XCR0.
//!
//! Every tier works in 64-byte blocks (eight words) and produces the same words, so a fill by one
//! tier can be verified by any other. Memory is touched only through volatile loads and stores
//! (non-temporal stores are intrinsics), so no access is elided or answered from a register.

#![no_std]

#[cfg(test)]
extern crate std;

mod block;
mod caps;
mod fault;
mod kernels;
mod ops;
mod prsg;
mod region;

pub use caps::{Caps, Kernel, Stores, Tier};
pub use fault::{CpuAddr, Fault, Faults};
pub use ops::{
    CopyError, Inversion, Order, copy, fill, fill_address, fill_prsg, invert, verify, verify_address, verify_prsg,
};
pub use prsg::Prsg;
pub use region::{BLOCK_BYTES, BLOCK_WORDS, Region, RegionError};

/// Chunk size for the wide tiers when they run with interrupts off: at a few GB/s it takes tens of
/// microseconds, well clear of the firmware's timer period.
pub const WIDE_CHUNK_BYTES: usize = 256 << 10;

#[cfg(test)]
mod tests;
