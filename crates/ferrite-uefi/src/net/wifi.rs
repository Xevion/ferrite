//! The appliance's Wi-Fi: the station service from `ferrite_wifi_efi`, whose port becomes one more
//! stack in the [`Network`](ferrite_net::Network) beside the wired ones.
//!
//! The station links the Linux Wi-Fi C, so only the UEFI build has it; a host build gets a station
//! that never starts, which keeps the rest of the app testable on the host.

use core::time::Duration;

use ferrite_efi::clock::Clock;
use ferrite_wifi::PortEvent;

#[cfg(ferrite_app)]
use ferrite_wifi_efi::runtime::{NextPoll, RuntimeConfig};
#[cfg(ferrite_app)]
use ferrite_wifi_efi::station::{StationConfig, WifiStation};

/// How long one poll may run the Linux side; the sweep's own slices are a millisecond.
#[cfg(ferrite_app)]
const BUDGET: Duration = Duration::from_millis(2);
/// How long the main loop may go without polling before the station parks the card. The longest
/// gap is at start-up, while the sweep walks the memory map and claims its ranges.
#[cfg(ferrite_app)]
const STALL_LIMIT: Duration = Duration::from_secs(10);

pub struct Wifi {
    #[cfg(ferrite_app)]
    station: Option<WifiStation>,
}

#[cfg(ferrite_app)]
impl Wifi {
    /// Start the station. Call before the sweep claims memory: the shim's arena comes from below 4 GiB.
    pub fn start(clock: Clock) -> Self {
        let config = StationConfig {
            runtime: RuntimeConfig { stall_limit: Some(STALL_LIMIT), ..RuntimeConfig::default() },
            ..StationConfig::default()
        };
        let station = WifiStation::start(clock, &config).inspect_err(|e| log::error!("wifi: {e}")).ok();
        Self { station }
    }

    /// Run the Linux side for a moment; how soon it wants the CPU again, `None` when it waits on nothing.
    pub fn poll(&mut self) -> Option<Duration> {
        match self.station.as_mut()?.poll(BUDGET) {
            NextPoll::Now => Some(Duration::ZERO),
            NextPoll::After(wait) => Some(wait),
            NextPoll::Idle => None,
        }
    }

    pub fn next_port_event(&self) -> Option<PortEvent> {
        self.station.as_ref()?.next_port_event()
    }

    /// Whether the station may still bring a link up: it started and has not ended.
    pub fn pending(&self) -> bool {
        self.station.as_ref().is_some_and(|station| !station.state().is_final())
    }

    pub fn label(&self) -> &'static str {
        self.station.as_ref().map_or("the Wi-Fi runtime did not start", WifiStation::label)
    }

    /// Leave the network and give the card and the shim's memory back.
    pub fn shutdown(&mut self) {
        if let Some(station) = self.station.take() {
            station.shutdown();
            log::info!("wifi: shut down");
        }
    }
}

#[cfg(not(ferrite_app))]
impl Wifi {
    pub fn start(_clock: Clock) -> Self {
        Self {}
    }

    #[expect(clippy::unused_self, clippy::needless_pass_by_ref_mut, reason = "the UEFI build's signature")]
    pub fn poll(&mut self) -> Option<Duration> {
        None
    }

    #[expect(clippy::unused_self, reason = "the UEFI build's signature")]
    pub fn next_port_event(&self) -> Option<PortEvent> {
        None
    }

    #[expect(clippy::unused_self, reason = "the UEFI build's signature")]
    pub fn pending(&self) -> bool {
        false
    }

    #[expect(clippy::unused_self, reason = "the UEFI build's signature")]
    pub fn label(&self) -> &'static str {
        "not in this build"
    }

    #[expect(clippy::unused_self, clippy::needless_pass_by_ref_mut, reason = "the UEFI build's signature")]
    pub fn shutdown(&mut self) {}
}
