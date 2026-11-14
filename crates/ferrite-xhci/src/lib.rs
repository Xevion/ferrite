//! xHCI Debug Capability (DbC) serial driver: a USB 3 debug serial device
//! presented by one root port of the host controller we run on.
//!
//! Written against the `ferrite-hal` traits, so it runs on any backend that
//! provides MMIO, PCI config access, DMA memory below 4 GiB and a clock. Finding
//! the controller on the bus is the backend's job: it hands over a PCI address,
//! [`XhciFunction::identify`] reads config space, [`XhciDevice::probe`] parses
//! the capabilities and [`Dbc::open`] brings the DbC up.

#![no_std]

extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod config;
pub mod dbc;
pub mod device;
mod dma;
mod endpoint;
pub mod error;
pub mod host;
pub mod pci;
pub mod regs;
mod ring;
pub mod stats;
pub mod trace;
pub mod trb;
mod watch;

pub use config::{Config, InTrbSize};
pub use dbc::{BLOCK_PAGES, Dbc, State, Written};
pub use device::{ExtCap, PortNumber, PortRange, XhciDevice, XhciFunction, ext_caps};
pub use error::{DmaError, Error};
pub use stats::{RegSnapshot, Stats, Timings};
