//! `ferrite-lab station`: the station service of `ferrite-wifi-start` against the simulated access point,
//! driven the way an app's main loop drives it. It must connect and offer a port, notice the access point
//! deauthenticating it, withdraw the port, back off, scan and connect again with a fresh port that carries
//! IP (a DHCP lease from the lab AP, pings, UDP and HTTP), back off from a stable connection as from a
//! first one, and stop on request with the state `Stopped`.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use ferrite_lkpi::sched::{NoHooks, Scheduler, Step};
use ferrite_wifi::{Failure, Policy, Shared, StationState};
use ferrite_wifi_start::station::{StationConfig, lab_credentials, lab_deauth_station, start_station};

use crate::host::{self, HostPlatform, MmapStacks};
use crate::net::MainNet;

const BUDGET: Duration = Duration::from_millis(10);
const STEP_LIMIT: Duration = Duration::from_secs(20);
/// The backoff the service starts at and the time a connection must stay up to reset it. The deauthentication
/// right after the first connection is far inside `stable`, the second connection is held past it.
const POLICY: Policy =
    Policy { initial: Duration::from_secs(1), max: Duration::from_secs(4), stable: Duration::from_secs(2) };

type LabScheduler = Scheduler<HostPlatform, MmapStacks, NoHooks>;

/// Poll `sched` and the main loop's IP until `done` holds, sleeping to the next deadline when every task
/// is parked and IP has nothing to do.
fn wait_for(
    sched: &mut LabScheduler,
    net: &mut MainNet,
    what: &str,
    mut done: impl FnMut(&LabScheduler, &MainNet) -> bool,
) -> Result<(), ExitCode> {
    let began = Instant::now();
    while !done(sched, net) {
        if began.elapsed() > STEP_LIMIT {
            log::error!("lab station: no {what} within {} s", STEP_LIMIT.as_secs());
            return Err(ExitCode::FAILURE);
        }
        let step = sched.poll(BUDGET);
        let ip_wait = net.poll(sched.handle().now());
        match step {
            Ok(Step { runnable: true, .. }) => {},
            Ok(_) if ip_wait == Some(Duration::ZERO) => {},
            Ok(Step { next_deadline: Some(at), .. }) => {
                let wait = at.saturating_duration_since(sched.handle().now()).min(Duration::from_millis(5));
                std::thread::sleep(ip_wait.map_or(wait, |ip| ip.min(wait)));
            },
            Ok(_) => std::thread::sleep(Duration::from_millis(1)),
            Err(error) => {
                log::error!("{error}");
                return Err(ExitCode::FAILURE);
            },
        }
    }
    Ok(())
}

pub fn run(sched: &mut LabScheduler) -> ExitCode {
    match drive(sched) {
        Ok(()) => ExitCode::SUCCESS,
        Err(code) => code,
    }
}

/// The wait before the next attempt, if the station is in the failed state a lost link leaves it in.
fn retry_after_loss(shared: &Shared) -> Option<Duration> {
    match shared.state() {
        StationState::Failed { reason: Failure::Lost, retry_in } => retry_in,
        _ => None,
    }
}

/// Have the AP deauthenticate the station, and wait for the port to be withdrawn and the station to
/// say how long it will back off.
fn lose_the_link(
    sched: &mut LabScheduler,
    net: &mut MainNet,
    shared: &Shared,
    which: &str,
) -> Result<Duration, ExitCode> {
    lab_deauth_station();
    wait_for(sched, net, "withdrawn port after the deauthentication", |_, net| !net.link_up())?;
    log::info!("lab station: port withdrawn while {}", shared.state());
    let Some(wait) = retry_after_loss(shared) else {
        log::error!("lab station: after the {which} deauthentication the state is {}", shared.state());
        return Err(ExitCode::FAILURE);
    };
    Ok(wait)
}

fn drive(sched: &mut LabScheduler) -> Result<(), ExitCode> {
    let Some(credentials) = lab_credentials() else {
        log::error!("lab station: the lab credentials are invalid");
        return Err(ExitCode::FAILURE);
    };
    let seed = host::stack_seed().map_err(|error| {
        log::error!("reading host entropy failed: {error}");
        ExitCode::FAILURE
    })?;
    let (shared, ap_link) = (Shared::new(), Shared::new());
    let config = StationConfig {
        sched: sched.handle(),
        credentials,
        regdb_limit: Duration::from_secs(2),
        wifi_limit: Duration::from_secs(10),
        scan_limit: Duration::from_secs(10),
        connect_limit: Duration::from_secs(10),
        reconnect: POLICY,
        lab_ap: Some(ap_link.clone()),
    };
    let task = start_station(config, shared.clone()).map_err(|code| {
        log::error!("starting the station failed: {code}");
        ExitCode::FAILURE
    })?;
    let id = sched.find_by_user(task.as_ptr() as usize).ok_or_else(|| {
        log::error!("the station has no scheduler task");
        ExitCode::FAILURE
    })?;
    let mut net = MainNet::new(shared.clone(), ap_link, seed);

    wait_for(sched, &mut net, "port from the station", |_, net| net.links() >= 1)?;
    log::info!("lab station: port up while {}", shared.state());
    if !shared.state().is_connected() {
        log::error!("lab station: a port was offered but the state is {}", shared.state());
        return Err(ExitCode::FAILURE);
    }

    let first_wait = lose_the_link(sched, &mut net, &shared, "first")?;
    if first_wait != POLICY.initial {
        log::error!("lab station: the first backoff is {first_wait:?}, not {:?}", POLICY.initial);
        return Err(ExitCode::FAILURE);
    }
    wait_for(sched, &mut net, "new attempt after the backoff", |_, _| {
        matches!(shared.state(), StationState::Scanning)
    })?;
    log::info!("lab station: reconnecting: {}", shared.state());

    wait_for(sched, &mut net, "second port after the reconnect", |_, net| {
        net.links() >= 2 && shared.verdict().is_some()
    })?;
    let reconnected = Instant::now();
    if shared.verdict() != Some(true) || !shared.state().is_connected() {
        log::error!("lab station: the second link failed its IP checks; the state is {}", shared.state());
        return Err(ExitCode::FAILURE);
    }
    log::info!("lab station: reconnected with a new port, which leased an address and passed the IP checks");

    wait_for(sched, &mut net, "stable connection", |_, _| {
        reconnected.elapsed() > POLICY.stable + Duration::from_millis(200)
    })?;
    let second_wait = lose_the_link(sched, &mut net, &shared, "second")?;
    if second_wait != POLICY.initial {
        log::error!(
            "lab station: after a connection that stayed up the backoff is {second_wait:?}, not back at {:?}",
            POLICY.initial
        );
        return Err(ExitCode::FAILURE);
    }
    log::info!("lab station: backoff reset to {second_wait:?} after the stable connection");
    wait_for(sched, &mut net, "new attempt after the second backoff", |_, _| {
        matches!(shared.state(), StationState::Scanning)
    })?;

    shared.request_stop();
    wait_for(sched, &mut net, "stop", |sched, _| sched.info(id).is_none())?;
    if shared.state() != StationState::Stopped || shared.next_port_event().is_some() {
        log::error!("lab station: after the stop the state is {}", shared.state());
        return Err(ExitCode::FAILURE);
    }
    log::info!("lab station: stopped; connect, deauthentication, reconnect, stable reconnect and stop passed");
    Ok(())
}
