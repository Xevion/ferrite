//! UEFI runtime support shared by every app.

#![no_std]

extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod bootlog;
pub mod clock;
pub mod logger;
pub mod session;
