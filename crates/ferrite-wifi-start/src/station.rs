//! The station service: the Wi-Fi C kept up as a network port for the program's main loop.
//!
//! One kernel thread brings the Linux side up (initcalls, the PCI card, its wireless interface), then
//! loops: scan (probing for the hidden networks by name), try the configured networks the scan found,
//! strongest first, until one joins, attach the netdev, offer a [`QueuePort`](ferrite_net::QueuePort)
//! through [`Shared`], and hold the connection, answering group rekeys, until the link drops or a stop is
//! requested. Only when every candidate of a scan has failed does the attempt end in a failure and a
//! backoff. A dropped link withdraws the port, detaches the netdev and reconnects with backoff, offering
//! a new port each time. A stop disconnects, detaches, takes the interface down and runs the exitcalls.
//! The thread never fails the program: whatever goes wrong ends in a [`StationState`](ferrite_wifi::StationState).
//!
//! The main loop owns the scheduler and polls it; this thread only runs inside those polls. Nothing in
//! it may be called from the main stack.

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::{String, ToString as _};
use core::ffi::{c_int, c_void};
use core::ptr::NonNull;
use core::time::Duration;

use ferrite_hal::Instant;
use ferrite_lkpi::sched::SchedHandle;
use ferrite_net::port::MacAddr;
use ferrite_nl80211::nl80211::{Frequency, SsidDisplay};
use ferrite_wifi::select::{self, Candidate};
use ferrite_wifi::{Failure, Next, Policy, Shared, StationState, Supervisor};
use ferrite_wpa::env::Credentials;

use crate::connect::{self, ConnectError};
use crate::lab_ap;
use crate::scan::{self, ScanError};
use crate::{
    LAB_PASSPHRASE, LAB_SSID, RegdbError, WirelessOp, bring_card_up, enter, lx_kernel_init, msleep, run_exitcalls,
    run_initcalls, spawn, wait_for_regdb, wireless,
};

/// What the station service needs from the program that starts it.
pub struct StationConfig {
    /// The scheduler the service runs on, for the clock the limits are measured with.
    pub sched: SchedHandle,
    /// The networks to join, tried strongest first.
    pub credentials: Credentials,
    /// How long cfg80211 gets to load `regulatory.db` after its initcalls; without it the service goes on
    /// with the built-in world domain.
    pub regdb_limit: Duration,
    /// How long the card gets from its probe to a registered wireless interface.
    pub wifi_limit: Duration,
    /// How long a scan gets from its trigger to its end.
    pub scan_limit: Duration,
    /// How long a connection gets from its request to the open port, and a disconnect to its event.
    pub connect_limit: Duration,
    pub reconnect: Policy,
    /// Run on the lab's simulated radios when there is no PCI card: the second radio becomes the access
    /// point of the [`LabAp`]'s network, scans look at its channel only, and the access point's own link
    /// is offered through its `link` for the lab's IP stack to serve DHCP on.
    pub lab_ap: Option<LabAp>,
}

/// How the lab's access point secures its network.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LabSecurity {
    Open,
    /// WPA2-PSK.
    Psk,
    /// WPA3-SAE with protected management frames required; `h2e` offers hash-to-element besides
    /// hunting and pecking.
    Sae {
        h2e: bool,
    },
    /// WPA2-PSK and WPA3-SAE on one network, with hash-to-element.
    Transition,
}

/// The network the lab's access point serves.
#[derive(Clone, Copy, Debug)]
pub struct LabNetwork {
    pub ssid: &'static [u8],
    /// The passphrase of a secured network; unused when it is open.
    pub passphrase: &'static [u8],
    pub security: LabSecurity,
    /// The beacons carry no name and the access point answers only a probe that names it.
    pub hidden: bool,
}

impl LabNetwork {
    /// The WPA2-PSK network `ferrite-lab` serves unless told otherwise.
    pub const PSK: Self =
        Self { ssid: LAB_SSID, passphrase: LAB_PASSPHRASE, security: LabSecurity::Psk, hidden: false };
}

/// The lab's access point: the network it serves, and where its own link is offered.
pub struct LabAp {
    pub link: Rc<Shared>,
    pub network: LabNetwork,
}

/// Make the lab's access point deauthenticate its station, as one that drops a client would.
pub fn lab_deauth_station() {
    lab_ap::request_deauth();
}

/// How often a pause between attempts looks at the stop request.
const PAUSE_SLICE_MS: u32 = 100;

struct Service {
    config: StationConfig,
    shared: Rc<Shared>,
}

/// Create the station thread and return its `task_struct`, so the program can see when it is gone.
/// The thread reports through `shared`, and stops when it asks (`Shared::request_stop`).
pub fn start_station(config: StationConfig, shared: Rc<Shared>) -> Result<NonNull<c_void>, c_int> {
    let arg = Box::into_raw(Box::new(Service { config, shared }));
    spawn(thread, arg.cast(), c"wifi-station", 0).inspect_err(|_| {
        // SAFETY: the thread was not created, so nothing else holds the box.
        drop(unsafe { Box::from_raw(arg) });
    })
}

unsafe extern "C" fn thread(arg: *mut c_void) -> c_int {
    // SAFETY: `start_station` passed the pointer of a leaked `Service` and nothing else uses it.
    let service = unsafe { Box::from_raw(arg.cast::<Service>()) };
    service.run();
    0
}

impl Service {
    fn run(self) {
        let sched = self.config.sched.clone();
        let clock = || sched.now();
        let mut supervisor = Supervisor::new(Rc::clone(&self.shared), self.config.reconnect);
        enter(c"kernel start-up");
        // SAFETY: called on this task, with the scheduler installed and kthreadd running, once.
        unsafe { lx_kernel_init() };
        enter(c"initcalls (PCI probes run here)");
        run_initcalls(log::Level::Debug);
        enter(c"regulatory.db wait");
        match wait_for_regdb(&clock, self.config.regdb_limit) {
            Ok(waited) => log::info!("regulatory.db applied by cfg80211 after {} ms", waited.as_millis()),
            Err(error @ RegdbError::NotLoaded { .. }) => {
                log::warn!("{error}; scans use the built-in world regulatory domain");
            },
            Err(RegdbError::AlreadyLoaded) => {},
        }
        if ferrite_lkpi::pci::device_count() > 0 {
            self.serve(&mut supervisor, &clock);
        } else if let Some(lab) = &self.config.lab_ap {
            self.serve_simulated(lab, &mut supervisor, &clock);
        } else {
            log::info!("no PCI Wi-Fi card was handed to the drivers");
            supervisor.unavailable(StationState::NoCard);
        }
        enter(c"exitcalls (driver removal)");
        run_exitcalls(log::Level::Debug);
        enter(c"done");
    }

    /// Bring the card up and keep the network joined until stopped or the card is gone.
    fn serve(&self, supervisor: &mut Supervisor, clock: &dyn Fn() -> Instant) {
        let Some(name) = bring_card_up(clock, self.config.wifi_limit) else {
            supervisor.failed(Failure::Driver, clock());
            return;
        };
        self.keep_joined(&name, &[], supervisor, clock);
        enter(c"bringing the wireless interface down");
        let (_, code) = wireless(WirelessOp::Down, 0);
        log::info!("{name} down ({code})");
    }

    /// The lab: the second simulated radio is the access point the first joins.
    fn serve_simulated(&self, lab: &LabAp, supervisor: &mut Supervisor, clock: &dyn Fn() -> Instant) {
        let offered = &*lab.link;
        let ((first, found_first), (second, found_second)) =
            (wireless(WirelessOp::Find, 0), wireless(WirelessOp::Find, 1));
        if found_first != 0 || found_second != 0 {
            log::error!("station: the simulated radios are not both registered");
            supervisor.failed(Failure::Driver, clock());
            return;
        }
        enter(c"starting the lab AP through nl80211");
        let ap = scan::connect().and_then(|mut client| {
            let interface = scan::prepare_ap(&mut client, &second, || match wireless(WirelessOp::Up, 1).1 {
                0 => Ok(()),
                code => Err(code),
            })?;
            let task =
                lab_ap::start(client, self.config.sched.clone(), interface.ifindex, interface.mac, &lab.network)?;
            Ok((task, interface.ifindex))
        });
        let (ap, ap_ifindex) = match ap {
            Ok(ap) => ap,
            Err(error) => {
                log::error!("station: starting the lab AP failed: {error}");
                supervisor.failed(Failure::Driver, clock());
                return;
            },
        };
        let (_, code) = wireless(WirelessOp::Up, 0);
        if code != 0 {
            log::error!("station: dev_open({first}) returned {code}");
            supervisor.failed(Failure::Driver, clock());
        } else {
            match crate::net::attach(ap_ifindex) {
                Ok(attachment) => {
                    offered.publish(attachment.link());
                    self.keep_joined(&first, &[Frequency(2412)], supervisor, clock);
                    offered.withdraw();
                    attachment.detach();
                },
                Err(code) => {
                    log::error!("station: attaching to the lab AP's frames failed: {code}");
                    supervisor.failed(Failure::Driver, clock());
                },
            }
        }
        ap.stop();
        for nth in [0, 1] {
            let (name, code) = wireless(WirelessOp::Down, nth);
            log::info!("{name} down ({code})");
        }
    }

    /// Scan and join one of the configured networks, again and again, until stopped or the failure is
    /// permanent.
    fn keep_joined(
        &self,
        name: &str,
        frequencies: &[Frequency],
        supervisor: &mut Supervisor,
        clock: &dyn Fn() -> Instant,
    ) {
        for network in self.config.credentials.networks() {
            log::info!("station: network {network}");
        }
        loop {
            if supervisor.stop_requested() {
                supervisor.stopped();
                break;
            }
            match self.attempt(name, frequencies, supervisor, clock) {
                Next::Done => break,
                Next::Retry(wait) => {
                    if !self.pause(wait, clock) {
                        supervisor.stopped();
                        break;
                    }
                },
            }
        }
    }

    /// Sleep `wait` on this task; false when a stop was requested meanwhile.
    fn pause(&self, wait: Duration, clock: &dyn Fn() -> Instant) -> bool {
        enter(c"waiting to reconnect");
        let until = clock() + wait;
        while clock() < until {
            if self.shared.stop_requested() {
                return false;
            }
            // SAFETY: on a task after the time bring-up.
            unsafe { msleep(PAUSE_SLICE_MS) };
        }
        !self.shared.stop_requested()
    }

    /// One scan, then the candidates it offers one after another until one connects, then the hold.
    /// Everything it opened is closed again when it returns, and the supervisor has been told how it
    /// ended: a failure and the backoff come only after every candidate has failed.
    fn attempt(
        &self,
        name: &str,
        frequencies: &[Frequency],
        supervisor: &mut Supervisor,
        clock: &dyn Fn() -> Instant,
    ) -> Next {
        let stop = || self.shared.stop_requested();
        let networks = self.config.credentials.networks();
        supervisor.scanning();
        enter(c"scanning through nl80211");
        let mut client = match scan::connect() {
            Ok(client) => client,
            Err(error) => return fail(supervisor, Failure::Scan, &error, clock),
        };
        let hidden = select::hidden_ssids(networks);
        let report = match scan::scan(
            &mut client,
            name,
            (frequencies, &hidden),
            self.config.scan_limit,
            &stop,
            log::Level::Debug,
            clock,
        ) {
            Ok(report) => report,
            Err(ScanError::Cancelled { .. }) => return supervisor.stopped(),
            Err(error) => return fail(supervisor, Failure::Scan, &error, clock),
        };
        let candidates = select::candidates(networks, &report.bsses);
        if candidates.is_empty() {
            log::warn!(
                "station: the scan found {} BSS and none that a configured network can join",
                report.bsses.len()
            );
            return supervisor.failed(Failure::NotFound, clock());
        }
        let interface = match scan::find(&mut client, name) {
            Ok(interface) => interface,
            Err(error) => return fail(supervisor, Failure::Scan, &error, clock),
        };
        let mut last = None;
        for candidate in &candidates {
            if supervisor.stop_requested() {
                return supervisor.stopped();
            }
            match self.connect_candidate(&mut client, &interface, candidate, supervisor, clock) {
                Ok(connection) => {
                    return self.hold(connection, &mut client, name, &interface, candidate, supervisor, clock);
                },
                Err(error) => {
                    connect::abort(&mut client, interface.ifindex, clock);
                    log::warn!(
                        "station: {} at {} failed: {error}{}",
                        SsidDisplay(candidate.network.ssid.as_bytes()),
                        candidate.bss.bssid,
                        if core::ptr::eq(candidate, candidates.last().unwrap_or(candidate)) {
                            ""
                        } else {
                            "; trying the next candidate"
                        }
                    );
                    last = Some(failure_of(&error));
                },
            }
        }
        supervisor.failed(last.unwrap_or(Failure::Protocol), clock())
    }

    fn connect_candidate(
        &self,
        client: &mut ferrite_nl80211::client::Client<scan::KernelSocket>,
        interface: &ferrite_nl80211::nl80211::Interface,
        candidate: &Candidate<'_>,
        supervisor: &Supervisor,
        clock: &dyn Fn() -> Instant,
    ) -> Result<connect::Connection, ConnectError> {
        let ssid = candidate.network.ssid.as_bytes();
        let signal = candidate.bss.signal.map_or_else(|| String::from("?"), |s| s.to_string());
        log::info!(
            "station: connecting to {} on {} MHz at {signal} ({:?})",
            candidate.bss.bssid,
            candidate.bss.frequency.0,
            candidate.join
        );
        supervisor.connecting(&SsidDisplay(ssid).to_string());
        enter(c"connecting (association and the 4-way handshake)");
        let derive = clock();
        let secret = connect::secret_for(candidate.network, candidate.join).ok_or(ConnectError::Unsupported)?;
        log::debug!("secret ready in {} ms", (clock() - derive).as_millis());
        connect::connect(
            client,
            interface.ifindex,
            interface.mac,
            candidate.bss,
            secret,
            self.config.connect_limit,
            clock,
        )
    }

    /// Offer the connection's port, hold it until the link drops or a stop is requested, and close it.
    #[expect(clippy::too_many_arguments, reason = "the attempt's state, split off to keep it readable")]
    fn hold(
        &self,
        mut connection: connect::Connection,
        client: &mut ferrite_nl80211::client::Client<scan::KernelSocket>,
        name: &str,
        interface: &ferrite_nl80211::nl80211::Interface,
        candidate: &Candidate<'_>,
        supervisor: &mut Supervisor,
        clock: &dyn Fn() -> Instant,
    ) -> Next {
        let stop = || self.shared.stop_requested();
        let ssid = candidate.network.ssid.as_bytes();
        if supervisor.stop_requested() {
            let next = supervisor.stopped();
            leave(connection, client, self.config.connect_limit, clock);
            return next;
        }
        // Power save holds frames to the station until the access point's next beacon, up to ~100 ms.
        match client.set_power_save(interface.ifindex, false) {
            Ok(()) => log::info!("station: power save off"),
            Err(error) => log::warn!("station: power save stays on: {error}"),
        }
        enter(c"attaching the netdev to the IP stack");
        let attachment = match crate::net::attach(interface.ifindex) {
            Ok(attachment) => attachment,
            Err(code) => {
                log::error!("station: attaching to {name}'s frames failed: {code}");
                let next = supervisor.failed(Failure::Attach, clock());
                leave(connection, client, self.config.connect_limit, clock);
                return next;
            },
        };
        supervisor.connected(&SsidDisplay(ssid).to_string(), MacAddr::new(connection.ap.0), attachment.link(), clock());
        log::info!(
            "station: connected to {} on {} MHz: associated after {} ms, handshake {} ms later",
            connection.ap,
            candidate.bss.frequency.0,
            connection.associated_after.as_millis(),
            connection.handshake_after.as_millis()
        );
        enter(c"holding the connection");
        let held = connection.watch(client, &stop, clock);
        let (next, left) = match held {
            Ok(()) => (supervisor.stopped(), true),
            Err(error) => {
                log::warn!("station: {error}");
                (supervisor.failed(Failure::Lost, clock()), false)
            },
        };
        attachment.detach();
        if left {
            leave(connection, client, self.config.connect_limit, clock);
        }
        next
    }
}

/// Log `error` as a warning and report the attempt as failed with `reason`.
fn fail(
    supervisor: &mut Supervisor,
    reason: Failure,
    error: &dyn core::fmt::Display,
    clock: &dyn Fn() -> Instant,
) -> Next {
    log::warn!("station: {error}");
    supervisor.failed(reason, clock())
}

fn failure_of(error: &ConnectError) -> Failure {
    match error {
        ConnectError::Rejected { .. } | ConnectError::SaeRejected { .. } | ConnectError::Disconnected { .. } => {
            Failure::Rejected
        },
        ConnectError::Expired { .. } => Failure::Timeout,
        ConnectError::Nl80211 { .. } | ConnectError::Sae { .. } | ConnectError::NoRsn | ConnectError::Unsupported => {
            Failure::Protocol
        },
    }
}

fn leave(
    connection: connect::Connection,
    client: &mut ferrite_nl80211::client::Client<scan::KernelSocket>,
    limit: Duration,
    clock: &dyn Fn() -> Instant,
) {
    enter(c"disconnecting");
    match connection.leave(client, limit, clock) {
        Ok(reason) => log::info!(
            "station: disconnected, reason {}",
            reason.map_or_else(|| String::from("none"), |reason| reason.to_string())
        ),
        Err(error) => log::warn!("station: leaving the connection failed: {error}"),
    }
}
