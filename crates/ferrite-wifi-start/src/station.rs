//! The station service: the Wi-Fi C kept up as a network port for the program's main loop.
//!
//! One kernel thread brings the Linux side up (initcalls, the PCI card, its wireless interface), then
//! loops: scan, join the configured network, attach the netdev, offer a [`QueuePort`](ferrite_net::QueuePort)
//! through [`Shared`], and hold the connection, answering group rekeys, until the link drops or a stop is
//! requested. A dropped link withdraws the port, detaches the netdev and reconnects with backoff, offering
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
use ferrite_wifi::{Failure, Next, Policy, Shared, StationState, Supervisor};
use ferrite_wpa::env::Credentials;
use ferrite_wpa::kdf::{Passphrase, Pmk, Ssid};

use crate::connect::{self, ConnectError};
use crate::lab_ap;
use crate::scan::{self, ScanError};
use crate::{
    LAB_PASSPHRASE, LAB_SSID, RegdbError, WirelessOp, best_bss, bring_card_up, enter, lx_kernel_init, msleep,
    run_exitcalls, run_initcalls, spawn, wait_for_regdb, wireless,
};

/// What the station service needs from the program that starts it.
pub struct StationConfig {
    /// The scheduler the service runs on, for the clock the limits are measured with.
    pub sched: SchedHandle,
    /// The network to join.
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
    /// point of [`lab_credentials`], scans look at its channel only, and the access point's own link is
    /// offered through this `Shared` for the lab's IP stack to serve DHCP on.
    pub lab_ap: Option<Rc<Shared>>,
}

/// The network the lab's access point serves.
pub fn lab_credentials() -> Option<Credentials> {
    Some(Credentials { ssid: Ssid::new(LAB_SSID).ok()?, passphrase: Passphrase::new(LAB_PASSPHRASE).ok()? })
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
        } else if let Some(offered) = &self.config.lab_ap {
            self.serve_simulated(offered, &mut supervisor, &clock);
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
    fn serve_simulated(&self, offered: &Shared, supervisor: &mut Supervisor, clock: &dyn Fn() -> Instant) {
        let ((first, found_first), (second, found_second)) =
            (wireless(WirelessOp::Find, 0), wireless(WirelessOp::Find, 1));
        if found_first != 0 || found_second != 0 {
            log::error!("station: the simulated radios are not both registered");
            supervisor.failed(Failure::Driver, clock());
            return;
        }
        let Some(credentials) = lab_credentials() else {
            log::error!("station: the lab credentials are invalid");
            supervisor.failed(Failure::Driver, clock());
            return;
        };
        let pmk = Pmk::from_passphrase(&credentials.passphrase, &credentials.ssid);
        enter(c"starting the lab AP through nl80211");
        let ap = scan::connect().and_then(|mut client| {
            let interface = scan::prepare_ap(&mut client, &second, || match wireless(WirelessOp::Up, 1).1 {
                0 => Ok(()),
                code => Err(code),
            })?;
            let task =
                lab_ap::start(client, self.config.sched.clone(), interface.ifindex, interface.mac, LAB_SSID, pmk)?;
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

    /// Scan and join `name`'s network, again and again, until stopped or the failure is permanent.
    fn keep_joined(
        &self,
        name: &str,
        frequencies: &[Frequency],
        supervisor: &mut Supervisor,
        clock: &dyn Fn() -> Instant,
    ) {
        let credentials = &self.config.credentials;
        log::info!(
            "station: network \"{}\", passphrase of {} characters",
            SsidDisplay(credentials.ssid.as_bytes()),
            credentials.passphrase.len()
        );
        let derive = clock();
        let pmk = Pmk::from_passphrase(&credentials.passphrase, &credentials.ssid);
        log::debug!("PMK derived in {} ms", (clock() - derive).as_millis());
        loop {
            if supervisor.stop_requested() {
                supervisor.stopped();
                break;
            }
            match self.attempt(name, frequencies, supervisor, &pmk, clock) {
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

    /// One scan, connect and hold. Everything it opened is closed again when it returns, and the
    /// supervisor has been told how it ended.
    fn attempt(
        &self,
        name: &str,
        frequencies: &[Frequency],
        supervisor: &mut Supervisor,
        pmk: &Pmk,
        clock: &dyn Fn() -> Instant,
    ) -> Next {
        let stop = || self.shared.stop_requested();
        let ssid = self.config.credentials.ssid.as_bytes();
        supervisor.scanning();
        enter(c"scanning through nl80211");
        let mut client = match scan::connect() {
            Ok(client) => client,
            Err(error) => return fail(supervisor, Failure::Scan, &error, clock),
        };
        let report =
            match scan::scan(&mut client, name, frequencies, self.config.scan_limit, &stop, log::Level::Debug, clock) {
                Ok(report) => report,
                Err(ScanError::Cancelled { .. }) => return supervisor.stopped(),
                Err(error) => return fail(supervisor, Failure::Scan, &error, clock),
            };
        let Some(bss) = best_bss(&report.bsses, ssid) else {
            log::warn!(
                "station: the scan found {} BSS and none offers WPA2 for \"{}\"",
                report.bsses.len(),
                SsidDisplay(ssid)
            );
            return supervisor.failed(Failure::NotFound, clock());
        };
        let signal = bss.signal.map_or_else(|| String::from("?"), |s| s.to_string());
        log::info!("station: connecting to {} on {} MHz at {signal}", bss.bssid, bss.frequency.0);
        supervisor.connecting(&SsidDisplay(ssid).to_string());
        enter(c"connecting (association and the 4-way handshake)");
        let interface = match scan::find(&mut client, name) {
            Ok(interface) => interface,
            Err(error) => return fail(supervisor, Failure::Scan, &error, clock),
        };
        let mut connection = match connect::connect(
            &mut client,
            interface.ifindex,
            interface.mac,
            bss,
            pmk.clone(),
            self.config.connect_limit,
            clock,
        ) {
            Ok(connection) => connection,
            Err(error) => {
                // Best effort: a refused or dropped connection is already down, and closing the client's
                // socket ends whatever is left of one.
                let _ = client.disconnect(interface.ifindex, ferrite_nl80211::mlme::ReasonCode::DeauthLeaving);
                return fail(supervisor, failure_of(&error), &error, clock);
            },
        };
        if supervisor.stop_requested() {
            let next = supervisor.stopped();
            leave(connection, &mut client, self.config.connect_limit, clock);
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
                leave(connection, &mut client, self.config.connect_limit, clock);
                return next;
            },
        };
        supervisor.connected(&SsidDisplay(ssid).to_string(), MacAddr::new(connection.ap.0), attachment.link(), clock());
        log::info!(
            "station: connected to {} on {} MHz: associated after {} ms, handshake {} ms later",
            connection.ap,
            bss.frequency.0,
            connection.associated_after.as_millis(),
            connection.handshake_after.as_millis()
        );
        enter(c"holding the connection");
        let held = connection.watch(&mut client, &stop, clock);
        let (next, left) = match held {
            Ok(()) => (supervisor.stopped(), true),
            Err(error) => {
                log::warn!("station: {error}");
                (supervisor.failed(Failure::Lost, clock()), false)
            },
        };
        attachment.detach();
        if left {
            leave(connection, &mut client, self.config.connect_limit, clock);
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
        ConnectError::Rejected { .. } | ConnectError::Disconnected { .. } => Failure::Rejected,
        ConnectError::Expired { .. } => Failure::Timeout,
        ConnectError::Nl80211 { .. } | ConnectError::NoRsn | ConnectError::Unsupported => Failure::Protocol,
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
