//! UEFI runtime support shared by every app.

#![no_std]

extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod bootlog;
pub mod clock;
#[cfg(feature = "config")]
pub mod config;
pub mod cores;
pub mod cpu;
#[cfg(feature = "crew")]
pub mod crew;
pub mod dma;
pub mod entropy;
pub mod link;
pub mod logger;
pub mod memory;
pub mod mp;
pub mod paging;
pub mod pci;
pub mod perf;
pub mod rtc;
pub mod sensors;
pub mod session;
#[cfg(feature = "net")]
pub mod snp;
pub mod volume;
pub mod watchdog;
pub mod xhci;
