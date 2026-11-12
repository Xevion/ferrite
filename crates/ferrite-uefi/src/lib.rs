//! Code shared between the main app and the standalone `src/bin/` apps.

#![no_std]

extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod dbc;
pub mod session;
