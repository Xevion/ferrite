//! Shared pieces of the recon apps: PCI decoding, the DbC bring-up the DbC apps use, report
//! formatting and the benchmark frame codec.

#![no_std]

extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod bench;
pub mod bringup;
pub mod caps;
pub mod display;
pub mod pci;
pub mod summary;
