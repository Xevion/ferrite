//! Generic netlink and nl80211 for a userspace-style client: message and attribute encoding and
//! decoding, controller family lookup, and a [`client::Client`] that resolves nl80211, finds the
//! wireless interfaces, triggers a scan, waits for it on the scan group and dumps the results.
//!
//! Platform-free: the socket is a trait, so the same client runs over the shim's in-process netlink
//! sockets in the lab and under UEFI, and over hand-built datagrams in the tests.

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod client;
pub mod genl;
pub mod netlink;
pub mod nl80211;
