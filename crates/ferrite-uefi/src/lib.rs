//! A bare-metal memory sweep with a framebuffer dashboard (ratatui on text-only consoles) and a
//! status page served over the machine's own NIC.

#![no_std]

extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod app;
pub mod backend;
pub mod control;
pub mod cores;
pub mod dash;
pub mod diag;
pub mod gfx;
pub mod gop;
pub mod net;
pub mod report;
pub mod sweep;
#[cfg(test)]
mod testing;
pub mod ui;
