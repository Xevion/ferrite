//! UEFI runtime support shared by every app.

#![no_std]

extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod bootlog;
pub mod clock;
pub mod dma;
pub mod entropy;
pub mod link;
pub mod logger;
pub mod pci;
pub mod session;
pub mod watchdog;
pub mod xhci;
