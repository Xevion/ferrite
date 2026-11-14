//! `ferrite-lab station`: the station service of `ferrite-wifi-start` against the simulated access point,
//! driven the way an app's main loop drives it. It must connect and offer a port, notice the access point
//! deauthenticating it, withdraw the port and start over, and stop on request with the state `Stopped`.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use ferrite_lkpi::sched::{NoHooks, Scheduler, Step};
use ferrite_wifi::{Failure, Policy, PortEvent, Shared, StationState};
use ferrite_wifi_start::station::{StationConfig, lab_credentials, lab_deauth_station, start_station};

use crate::host::{HostPlatform, MmapStacks};

const BUDGET: Duration = Duration::from_millis(10);
const STEP_LIMIT: Duration = Duration::from_secs(20);

type LabScheduler = Scheduler<HostPlatform, MmapStacks, NoHooks>;

/// Poll `sched` until `done` holds, sleeping to the next deadline when every task is parked.
fn wait_for(sched: &mut LabScheduler, what: &str, mut done: impl FnMut(&LabScheduler) -> bool) -> Result<(), ExitCode> {
    let began = Instant::now();
    while !done(sched) {
        if began.elapsed() > STEP_LIMIT {
            log::error!("lab station: no {what} within {} s", STEP_LIMIT.as_secs());
            return Err(ExitCode::FAILURE);
        }
        match sched.poll(BUDGET) {
            Ok(Step { runnable: true, .. }) => {},
            Ok(Step { next_deadline: Some(at), .. }) => {
                std::thread::sleep(at.saturating_duration_since(sched.handle().now()).min(Duration::from_millis(5)));
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

fn drive(sched: &mut LabScheduler) -> Result<(), ExitCode> {
    let Some(credentials) = lab_credentials() else {
        log::error!("lab station: the lab credentials are invalid");
        return Err(ExitCode::FAILURE);
    };
    let shared = Shared::new();
    let config = StationConfig {
        sched: sched.handle(),
        credentials,
        regdb_limit: Duration::from_secs(2),
        wifi_limit: Duration::from_secs(10),
        scan_limit: Duration::from_secs(10),
        connect_limit: Duration::from_secs(10),
        reconnect: Policy {
            initial: Duration::from_millis(500),
            max: Duration::from_secs(2),
            stable: Duration::from_secs(60),
        },
        simulated: true,
    };
    let task = start_station(config, shared.clone()).map_err(|code| {
        log::error!("starting the station failed: {code}");
        ExitCode::FAILURE
    })?;
    let id = sched.find_by_user(task.as_ptr() as usize).ok_or_else(|| {
        log::error!("the station has no scheduler task");
        ExitCode::FAILURE
    })?;

    let mut port = None;
    wait_for(sched, "port from the station", |_| {
        if let Some(PortEvent::Up(up)) = shared.next_port_event() {
            port = Some(up);
        }
        port.is_some()
    })?;
    log::info!("lab station: port up while {}", shared.state());
    if !shared.state().is_connected() {
        log::error!("lab station: a port was offered but the state is {}", shared.state());
        return Err(ExitCode::FAILURE);
    }

    lab_deauth_station();
    wait_for(sched, "withdrawn port after the deauthentication", |_| {
        matches!(shared.next_port_event(), Some(PortEvent::Down))
    })?;
    log::info!("lab station: port withdrawn while {}", shared.state());
    if !matches!(shared.state(), StationState::Failed { reason: Failure::Lost, retry_in: Some(_) }) {
        log::error!("lab station: after the deauthentication the state is {}", shared.state());
        return Err(ExitCode::FAILURE);
    }
    wait_for(sched, "new attempt after the backoff", |_| matches!(shared.state(), StationState::Scanning))?;
    log::info!("lab station: reconnecting: {}", shared.state());

    shared.request_stop();
    wait_for(sched, "stop", |sched| sched.info(id).is_none())?;
    if shared.state() != StationState::Stopped || shared.next_port_event().is_some() {
        log::error!("lab station: after the stop the state is {}", shared.state());
        return Err(ExitCode::FAILURE);
    }
    log::info!("lab station: stopped; connect, deauthentication, reconnect attempt and stop passed");
    Ok(())
}
