//! Wi-Fi as a network port for an app.
//!
//! [`WifiStation`] brings the Linux Wi-Fi stack up on the first supported card, joins the network
//! the settings file names, and offers a [`QueuePort`](ferrite_net::QueuePort) once the link is open, withdrawing
//! it when the link drops and offering a new one after the reconnect.
//!
//! It never fails the app. A machine with no supported card, no credentials or missing firmware files
//! gets a station that reports why in [`WifiStation::state`] and holds no memory: the shim, and the
//! 64 MiB arena below 4 GiB it needs, come up only once those checks pass. If the Linux side later dies
//! (the card fails, a stub is hit) the station reports it, withdraws its port and gives the arena back.
//!
//! The app polls [`WifiStation::poll`] from its main loop on the bootstrap processor, every few
//! milliseconds, and reads [`WifiStation::next_port_event`] to start and drop its IP stack.

use alloc::boxed::Box;
use alloc::rc::Rc;
use core::ffi::{c_int, c_void};
use core::ptr::NonNull;
use core::time::Duration;

use ferrite_efi::clock::Clock;
use ferrite_lkpi::firmware::FirmwareProvider as _;
use ferrite_lkpi::pci::PciPlatform as _;
use ferrite_wifi::{Failure, Policy, PortEvent, Shared, StationState};
use ferrite_wifi_start::station::start_station;
use ferrite_wpa::env::Credentials;
use snafu::Snafu;

use crate::firmware::VolumeFirmware;
use crate::pci::UefiPci;
use crate::platform::IRQ_POLL;
use crate::runtime::{NextPoll, Runtime, RuntimeConfig, RuntimeError, halted};

/// The regulatory database every connection needs, beside the card's own firmware.
const REGDB: &str = "regulatory.db";

/// What a station is started with.
#[derive(Clone, Copy, Debug)]
pub struct StationConfig {
    pub runtime: RuntimeConfig,
    /// How long cfg80211 gets to load `regulatory.db`; the station goes on without it afterwards.
    pub regdb_limit: Duration,
    /// How long the card gets from its probe to a registered interface: firmware read, INIT image, NVM.
    pub wifi_limit: Duration,
    /// How long a scan gets from its trigger to its end.
    pub scan_limit: Duration,
    /// How long a connection gets from its request to the open port, and a disconnect to its event.
    pub connect_limit: Duration,
    pub reconnect: Policy,
    /// How long [`WifiStation::shutdown`] waits for the service to leave the network before it tears the
    /// stack down regardless.
    pub stop_limit: Duration,
}

impl Default for StationConfig {
    fn default() -> Self {
        Self {
            runtime: RuntimeConfig::default(),
            regdb_limit: Duration::from_secs(5),
            wifi_limit: Duration::from_secs(15),
            scan_limit: Duration::from_secs(10),
            connect_limit: Duration::from_secs(10),
            reconnect: Policy::default(),
            stop_limit: Duration::from_secs(25),
        }
    }
}

#[derive(Debug, Snafu)]
pub enum StartError {
    #[snafu(display("the Wi-Fi runtime did not start: {source}"))]
    Runtime { source: RuntimeError },
    #[snafu(display("starting the station task failed: {code}"))]
    Task { code: c_int },
}

struct Running {
    runtime: Runtime,
    task: NonNull<c_void>,
}

/// The Wi-Fi station, owned by the app.
pub struct WifiStation {
    shared: Rc<Shared>,
    /// `None` when there is nothing to run, or once the Linux side has ended.
    running: Option<Running>,
    stop_limit: Duration,
}

/// Why a station cannot be started, found before any memory is taken: the card, the networks or a
/// firmware file is missing.
fn preflight(pci: &UefiPci, credentials: Option<Credentials>) -> Result<Credentials, StationState> {
    if pci.devices().is_empty() {
        log::info!("wifi: no supported card");
        return Err(StationState::NoCard);
    }
    let Some(credentials) = credentials else {
        log::info!("wifi: no networks configured");
        return Err(StationState::NoCredentials);
    };
    for name in core::iter::once(REGDB).chain(pci.firmware()) {
        VolumeFirmware.load(name).map_err(|error| {
            log::warn!("wifi: {error}");
            StationState::NoFirmware
        })?;
    }
    Ok(credentials)
}

impl WifiStation {
    /// Bring the station up on `clock`, to join the `networks` the settings file names. `Ok` also covers
    /// a machine that cannot run it: the state says why, and the app carries on without Wi-Fi. `Err` is
    /// a failure to bring the Linux side up.
    pub fn start(clock: Clock, config: &StationConfig, networks: Option<Credentials>) -> Result<Self, StartError> {
        let shared = Shared::new();
        let pci: &'static UefiPci = Box::leak(Box::new(UefiPci::discover()));
        let credentials = match preflight(pci, networks) {
            Ok(credentials) => credentials,
            Err(state) => {
                shared.set_state(state);
                return Ok(Self { shared, running: None, stop_limit: config.stop_limit });
            },
        };
        let mut runtime =
            Runtime::start_with(clock, &config.runtime, pci).map_err(|source| StartError::Runtime { source })?;
        let service = ferrite_wifi_start::station::StationConfig {
            sched: runtime.sched(),
            credentials,
            regdb_limit: config.regdb_limit,
            wifi_limit: config.wifi_limit,
            scan_limit: config.scan_limit,
            connect_limit: config.connect_limit,
            reconnect: config.reconnect,
            lab_ap: None,
        };
        let offered = Rc::clone(&shared);
        let task = runtime
            .on_task(move || start_station(service, offered))
            .map_err(|source| StartError::Runtime { source })?
            .map_err(|code| StartError::Task { code })?;
        Ok(Self { shared, running: Some(Running { runtime, task }), stop_limit: config.stop_limit })
    }

    /// What the station is doing.
    pub fn state(&self) -> StationState {
        self.shared.state()
    }

    /// [`StationState::label`] of what the station is doing, for a line drawn every frame.
    pub fn label(&self) -> &'static str {
        self.shared.label()
    }

    /// The state and port the service shares with the app, for a reader that outlives the station (a
    /// status page keeps reading after [`WifiStation::shutdown`]).
    pub fn shared(&self) -> Rc<Shared> {
        Rc::clone(&self.shared)
    }

    /// A port that opened or went away since the last call. On `Up` the app starts its stack over the
    /// port; on `Down` it drops that stack.
    pub fn next_port_event(&self) -> Option<PortEvent> {
        self.shared.next_port_event()
    }

    /// Run the Linux side for at most `budget` (a few milliseconds), and say how soon to poll again.
    /// After the station has ended this does nothing and returns [`NextPoll::Idle`].
    pub fn poll(&mut self, budget: Duration) -> NextPoll {
        let Some(running) = self.running.as_mut() else { return NextPoll::Idle };
        if let Some(stub) = halted() {
            log::error!("wifi: the Linux side stopped in stub {}", stub.to_string_lossy());
            self.end(StationState::Failed { reason: Failure::Driver, retry_in: None });
            return NextPoll::Idle;
        }
        match running.runtime.poll(budget) {
            Ok(next) if running.runtime.task_state(running.task).is_some() => next,
            Ok(_) => {
                let state = self.shared.state();
                log::info!("wifi: the station ended: {state}");
                self.end(state);
                NextPoll::Idle
            },
            Err(error) => {
                log::error!("wifi: {error}");
                self.end(StationState::Failed { reason: Failure::Driver, retry_in: None });
                NextPoll::Idle
            },
        }
    }

    /// Tear the Linux side down: the port is withdrawn, the state is `state`, the cards are off the bus
    /// and the memory is the firmware's again.
    fn end(&mut self, state: StationState) {
        self.shared.withdraw();
        self.shared.set_state(state);
        if let Some(running) = self.running.take() {
            running.runtime.shutdown();
        }
    }

    /// Leave the network, remove the drivers, and give the memory back. Polls the Linux side until the
    /// service is gone, at most `stop_limit`.
    pub fn shutdown(mut self) {
        let Some(mut running) = self.running.take() else { return };
        self.shared.request_stop();
        let began = running.runtime.now();
        while running.runtime.task_state(running.task).is_some() && halted().is_none() {
            if running.runtime.now() - began >= self.stop_limit {
                log::warn!("wifi: the station did not stop within {} s; tearing it down", self.stop_limit.as_secs());
                break;
            }
            match running.runtime.poll(Duration::from_millis(10)) {
                Ok(NextPoll::Now) => {},
                Ok(NextPoll::After(wait)) => running.runtime.idle(wait),
                Ok(NextPoll::Idle) => running.runtime.idle(IRQ_POLL),
                Err(error) => {
                    log::error!("wifi: {error}");
                    break;
                },
            }
        }
        self.shared.withdraw();
        if !self.shared.state().is_final() {
            self.shared.set_state(StationState::Stopped);
        }
        running.runtime.shutdown();
    }
}
