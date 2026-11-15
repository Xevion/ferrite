//! What `[time]` configures once it is checked: whether to keep the clock right over the network, which
//! servers to ask first, and how times are shown.

use alloc::vec::Vec;

use ferrite_net::notify::Host;
use jiff::tz::{TimeZone, TimeZoneDatabase};
use serde::Deserialize;

/// How a time of day is written.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, strum::IntoStaticStr)]
pub enum ClockFormat {
    /// `01:27:43 AM`.
    #[default]
    #[serde(rename = "12h")]
    #[strum(serialize = "12h")]
    Twelve,
    /// `13:27:43`.
    #[serde(rename = "24h")]
    #[strum(serialize = "24h")]
    TwentyFour,
}

/// The validated `[time]` section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimeSettings {
    /// Keep the clock right over the network; false sends nothing.
    pub ntp: bool,
    /// Asked before the servers of the DHCP lease and the public pool.
    pub servers: Vec<Host>,
    /// Where times are shown; UTC unless the file says.
    pub zone: TimeZone,
    pub clock: ClockFormat,
}

impl Default for TimeSettings {
    fn default() -> Self {
        Self { ntp: true, servers: Vec::new(), zone: TimeZone::UTC, clock: ClockFormat::default() }
    }
}

/// The time zone the bundled tz database knows by `name` (an IANA name, such as `America/Chicago`, in
/// any case). The database is the one in the binary, so the answer is the same on every machine.
#[must_use]
pub fn zone_named(name: &str) -> Option<TimeZone> {
    TimeZoneDatabase::bundled().get(name).ok().filter(|zone| !zone.is_unknown())
}
