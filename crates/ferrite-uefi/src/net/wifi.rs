//! The appliance's Wi-Fi: the station service from `ferrite_wifi_efi`, whose port becomes one more
//! stack in the [`Network`](ferrite_net::Network) beside the wired ones.
//!
//! The station links the Linux Wi-Fi C, so only the UEFI build has it; a host build gets a station
//! that never starts, which keeps the rest of the app testable on the host.

use alloc::rc::Rc;
use core::time::Duration;

use ferrite_efi::clock::Clock;
use ferrite_wifi::{PortEvent, Shared, StationState};
use ferrite_wpa::env::Credentials;

#[cfg(ferrite_app)]
use ferrite_wifi_efi::runtime::{NextPoll, RuntimeConfig};
#[cfg(ferrite_app)]
use ferrite_wifi_efi::station::{StationConfig, WifiStation};

#[cfg(ferrite_app)]
const RUNTIME_DOWN: &str = "the Wi-Fi runtime did not start";

/// How long one poll may run the Linux side; the sweep's own slices are a millisecond.
#[cfg(ferrite_app)]
const BUDGET: Duration = Duration::from_millis(2);
/// How long the main loop may go without polling before the station parks the card. The longest
/// gap is at start-up, while the sweep walks the memory map and claims its ranges.
#[cfg(ferrite_app)]
const STALL_LIMIT: Duration = Duration::from_secs(10);

/// What the status document says of the station at one moment, borrowed from its state.
pub struct WifiFacts<'a> {
    /// [`StationState::label`].
    pub state: &'static str,
    /// The network being joined or joined: set while connecting and connected.
    pub ssid: Option<&'a str>,
    /// Whole seconds (at least 1) until the next attempt, for a failure that has one.
    pub retry_in_s: Option<u64>,
    /// Whether the state will never change again.
    pub is_final: bool,
}

/// What the document shows of the station that changes: the facts less the network's name, which a
/// state change always moves with.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct WifiKey {
    state: &'static str,
    retry_in_s: Option<u64>,
}

/// A reader of the station's state for the status page, valid beyond the station's own life.
#[derive(Clone)]
pub struct WifiView {
    shared: Option<Rc<Shared>>,
    /// The state to report with no station behind the view.
    absent: &'static str,
}

impl WifiView {
    pub const fn new(shared: Rc<Shared>) -> Self {
        Self { shared: Some(shared), absent: "" }
    }

    /// A view with no station, always reporting `state` as a final one.
    pub const fn absent(state: &'static str) -> Self {
        Self { shared: None, absent: state }
    }

    /// Read the facts in place: no clone of the state, no allocation.
    pub fn read<R>(&self, read: impl FnOnce(WifiFacts<'_>) -> R) -> R {
        let Some(shared) = &self.shared else {
            return read(WifiFacts { state: self.absent, ssid: None, retry_in_s: None, is_final: true });
        };
        shared.with_state(|state| {
            let (ssid, retry_in_s) = match state {
                StationState::Connecting { ssid } | StationState::Connected { ssid, .. } => (Some(ssid.as_str()), None),
                StationState::Failed { retry_in: Some(wait), .. } => (None, Some(wait.as_secs().max(1))),
                _ => (None, None),
            };
            read(WifiFacts { state: state.label(), ssid, retry_in_s, is_final: state.is_final() })
        })
    }

    pub fn key(&self) -> WifiKey {
        self.read(|facts| WifiKey { state: facts.state, retry_in_s: facts.retry_in_s })
    }
}

pub struct Wifi {
    #[cfg(ferrite_app)]
    station: Option<WifiStation>,
}

#[cfg(ferrite_app)]
impl Wifi {
    /// Start the station to join `networks`. Call before the sweep claims memory: the shim's arena
    /// comes from below 4 GiB.
    pub fn start(clock: Clock, networks: Option<Credentials>) -> Self {
        let config = StationConfig {
            runtime: RuntimeConfig { stall_limit: Some(STALL_LIMIT), ..RuntimeConfig::default() },
            ..StationConfig::default()
        };
        let station = WifiStation::start(clock, &config, networks).inspect_err(|e| log::error!("wifi: {e}")).ok();
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
        self.station.as_ref().map_or(RUNTIME_DOWN, WifiStation::label)
    }

    /// The reader the status page keeps; it goes on reading the final state after [`Wifi::shutdown`].
    pub fn view(&self) -> WifiView {
        self.station.as_ref().map_or_else(|| WifiView::absent(RUNTIME_DOWN), |station| WifiView::new(station.shared()))
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
const NOT_IN_BUILD: &str = "not in this build";

#[cfg(not(ferrite_app))]
impl Wifi {
    pub fn start(_clock: Clock, _networks: Option<Credentials>) -> Self {
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
        NOT_IN_BUILD
    }

    #[expect(clippy::unused_self, reason = "the UEFI build's signature")]
    pub fn view(&self) -> WifiView {
        WifiView::absent(NOT_IN_BUILD)
    }

    #[expect(clippy::unused_self, clippy::needless_pass_by_ref_mut, reason = "the UEFI build's signature")]
    pub fn shutdown(&mut self) {}
}
