//! A bare-metal memory sweep with a ratatui UI and a status page served over
//! the machine's own NIC.

#![no_std]

extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod app;
pub mod backend;
pub mod control;
pub mod cores;
pub mod crew;
pub mod diag;
pub mod mdns;
pub mod net;
pub mod report;
pub mod sweep;
pub mod ui;
