//! The settings file on the boot volume, `\EFI\ferrite\ferrite.toml`: its schema, how it is read and
//! checked, and the template. The apps load it once at start; the boot-time wizard and the desktop tool
//! edit it through this crate, so every editor accepts and refuses exactly the same files.
//!
//! ```toml
//! [[wifi.networks]]
//! ssid = "Home"
//! passphrase = "correct horse battery"
//!
//! [[wifi.networks]]
//! ssid = "Cafe guest"
//! security = "open"
//!
//! [[wifi.networks]]
//! ssid = "lab-ap"
//! passphrase = "another passphrase"
//! security = "wpa3"
//! hidden = true
//!
//! [notify]
//! url = "http://ntfy.sh/ferrite-topic"
//! format = "text"
//! events = ["pass", "fault", "end"]
//!
//! [run]
//! passes = 3
//! time = "2h"
//! stop_on_fault = true
//! ```
//!
//! Unknown keys and sections are errors, so a typo is caught instead of ignored. Validation reuses the
//! constructors of the types the apps run on ([`ferrite_wpa::kdf::Ssid`] and
//! [`ferrite_wpa::kdf::Passphrase`], [`ferrite_net::notify::Config`], [`parse_duration`]), which is why
//! this crate depends on `ferrite-wpa` and `ferrite-net` and not the other way round.
//!
//! An error names the line, the column and the key, never the value: [`ConfigError`]. Each section is
//! read on its own ([`load`]), so a mistake in one leaves the others in force.

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

mod convert;
mod error;
mod load;
mod render;
mod run;
mod schema;
#[cfg(test)]
mod tests;

pub use convert::MAX_NETWORKS;
pub use error::{ConfigError, Path, Problem, Segment};
pub use load::{Loaded, load};
pub use run::{Compact, RunGoal, parse_duration};
pub use schema::{Config, Network, Notify, NotifyEvent, NotifyFormat, Run, Secret, Security, Wifi};

/// Where the file lives on the boot volume.
pub const PATH: &str = "\\EFI\\ferrite\\ferrite.toml";
