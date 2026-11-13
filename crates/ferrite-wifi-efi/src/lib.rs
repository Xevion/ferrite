//! The UEFI side of the Wi-Fi stack: the platform pieces `wifitrace` installs under the shim, each
//! from firmware services. The Linux C and its start-up sequence are shared with the lab.

#![no_std]

extern crate alloc;

pub mod arena;
pub mod firmware;
pub mod platform;
pub mod stacks;
