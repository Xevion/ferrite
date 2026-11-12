//! Shared pieces of the recon apps: PCI decoding, the DbC bring-up the DbC apps use and report formatting.

#![no_std]

extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod bringup;
pub mod display;
pub mod pci;
pub mod summary;
