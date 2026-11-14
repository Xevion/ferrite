//! The Wi-Fi station as the rest of the program sees it, platform-free: what it is doing
//! ([`StationState`]), how it reconnects ([`Reconnect`]), the hand-off of its network port to the main
//! loop ([`Shared`], [`PortEvent`]), which of the configured networks to try for a scan's results
//! ([`select`]) and how the main loop wakes the thread that sends its frames ([`Sleeper`]).
//!
//! The station service runs as a kernel task in `ferrite-wifi-start` and drives a [`Supervisor`]; the app's
//! main loop holds the same [`Shared`] and reads the state and the port events. Both sides run on one CPU
//! and never at the same time, so nothing here is atomic.

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

mod reconnect;
pub mod select;
mod shared;
mod state;
mod supervisor;
mod wake;

pub use reconnect::{Policy, Reconnect};
pub use shared::{PortEvent, Shared};
pub use state::{Failure, StationState};
pub use supervisor::{Next, Supervisor};
pub use wake::Sleeper;
