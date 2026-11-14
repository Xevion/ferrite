//! `ferrite-lab station [scenario]`: the station service of `ferrite-wifi-start` against the simulated
//! access point, driven the way an app's main loop drives it.
//!
//! Without a scenario it must connect to a WPA2-PSK network and offer a port, notice the access point
//! deauthenticating it, withdraw the port, back off, scan and connect again with a fresh port that carries
//! IP (a DHCP lease from the lab AP, pings, UDP and HTTP), back off from a stable connection as from a
//! first one, and stop on request with the state `Stopped`.
//!
//! A named scenario serves a different network from the access point and gives the station a `.env` file
//! to match: open, hidden, WPA3-SAE, a transition network, several networks of which the first fail. Each
//! must join, lease an address and pass the IP checks over the link, and stop.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use ferrite_lkpi::sched::{NoHooks, Scheduler, Step};
use ferrite_wifi::{Failure, Policy, Shared, StationState};
use ferrite_wifi_start::station::{LabAp, LabNetwork, LabSecurity, StationConfig, lab_deauth_station, start_station};
use ferrite_wpa::env::Credentials;

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

/// What a scenario has the station do.
enum Expect {
    /// Join, lease an address and pass the IP checks, and stop.
    Joins,
    /// Never join: the network is not found (the scan does not ask for its name) or its handshake fails.
    Fails,
}

/// A network the access point serves, the `.env` text the station is given, and what must happen.
struct Scenario {
    name: &'static str,
    ap: LabNetwork,
    env: &'static str,
    connect_limit: Duration,
    expect: Expect,
}

const OPEN_SSID: &[u8] = b"ferrite-open";
const HIDDEN_SSID: &[u8] = b"ferrite-hidden";
const SAE_SSID: &[u8] = b"ferrite-sae";

const SCENARIOS: [Scenario; 9] = [
    Scenario {
        name: "open",
        ap: LabNetwork { ssid: OPEN_SSID, passphrase: b"", security: LabSecurity::Open, hidden: false },
        env: "WPA_SSID=ferrite-open\nWPA_KEY_MGMT=NONE\n",
        connect_limit: Duration::from_secs(10),
        expect: Expect::Joins,
    },
    Scenario {
        name: "hidden",
        ap: LabNetwork {
            ssid: HIDDEN_SSID,
            passphrase: b"hidden lab passphrase",
            security: LabSecurity::Psk,
            hidden: true,
        },
        env: "WPA_SSID=ferrite-hidden\nWPA_PASSPHRASE=hidden lab passphrase\nWPA_HIDDEN=1\n",
        connect_limit: Duration::from_secs(10),
        expect: Expect::Joins,
    },
    Scenario {
        name: "hidden-unmarked",
        ap: LabNetwork {
            ssid: HIDDEN_SSID,
            passphrase: b"hidden lab passphrase",
            security: LabSecurity::Psk,
            hidden: true,
        },
        env: "WPA_SSID=ferrite-hidden\nWPA_PASSPHRASE=hidden lab passphrase\n",
        connect_limit: Duration::from_secs(10),
        expect: Expect::Fails,
    },
    Scenario {
        name: "sae",
        ap: LabNetwork {
            ssid: SAE_SSID,
            passphrase: b"sae lab passphrase",
            security: LabSecurity::Sae { h2e: true },
            hidden: false,
        },
        env: "WPA_SSID=ferrite-sae\nWPA_PASSPHRASE=sae lab passphrase\n",
        connect_limit: Duration::from_secs(10),
        expect: Expect::Joins,
    },
    Scenario {
        name: "sae-hunt",
        ap: LabNetwork {
            ssid: SAE_SSID,
            passphrase: b"sae lab passphrase",
            security: LabSecurity::Sae { h2e: false },
            hidden: false,
        },
        env: "WPA_SSID=ferrite-sae\nWPA_PASSPHRASE=sae lab passphrase\nWPA_KEY_MGMT=SAE\n",
        connect_limit: Duration::from_secs(10),
        expect: Expect::Joins,
    },
    Scenario {
        name: "transition",
        ap: LabNetwork {
            ssid: SAE_SSID,
            passphrase: b"sae lab passphrase",
            security: LabSecurity::Transition,
            hidden: false,
        },
        env: "WPA_SSID=ferrite-sae\nWPA_PASSPHRASE=sae lab passphrase\n",
        connect_limit: Duration::from_secs(10),
        expect: Expect::Joins,
    },
    Scenario {
        name: "transition-psk",
        ap: LabNetwork {
            ssid: SAE_SSID,
            passphrase: b"sae lab passphrase",
            security: LabSecurity::Transition,
            hidden: false,
        },
        env: "WPA_SSID=ferrite-sae\nWPA_PASSPHRASE=sae lab passphrase\nWPA_KEY_MGMT=WPA-PSK\n",
        connect_limit: Duration::from_secs(10),
        expect: Expect::Joins,
    },
    Scenario {
        name: "multi",
        ap: LabNetwork::PSK,
        env: "WPA_SSID=not-in-the-air\nWPA_PASSPHRASE=nobody home here\n\
              WPA_SSID_1=ferrite-lab\nWPA_PASSPHRASE_1=the wrong passphrase\n\
              WPA_SSID_2=ferrite-lab\nWPA_PASSPHRASE_2=ferrite lab passphrase\n",
        connect_limit: Duration::from_secs(3),
        expect: Expect::Joins,
    },
    Scenario {
        name: "sae-wrong-password",
        ap: LabNetwork {
            ssid: SAE_SSID,
            passphrase: b"sae lab passphrase",
            security: LabSecurity::Sae { h2e: true },
            hidden: false,
        },
        env: "WPA_SSID=ferrite-sae\nWPA_PASSPHRASE=another passphrase\n",
        connect_limit: Duration::from_secs(3),
        expect: Expect::Fails,
    },
];

pub fn run(sched: &mut LabScheduler, scenario: Option<&str>) -> ExitCode {
    let result = match scenario {
        None => drive(sched),
        Some(name) => SCENARIOS.iter().find(|s| s.name == name).map_or_else(
            || {
                let names: Vec<&str> = SCENARIOS.iter().map(|s| s.name).collect();
                log::error!("lab station: no scenario {name}; they are {}", names.join(", "));
                Err(ExitCode::FAILURE)
            },
            |scenario| drive_scenario(sched, scenario),
        ),
    };
    match result {
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

fn lab_env() -> String {
    let network = LabNetwork::PSK;
    format!(
        "WPA_SSID={}\nWPA_PASSPHRASE={}\n",
        String::from_utf8_lossy(network.ssid),
        String::from_utf8_lossy(network.passphrase)
    )
}

/// Join the network a scenario serves with the `.env` file it names, or fail to for the scenarios that
/// must not, then stop.
fn drive_scenario(sched: &mut LabScheduler, scenario: &Scenario) -> Result<(), ExitCode> {
    let credentials = Credentials::from_env(scenario.env.as_bytes()).map_err(|error| {
        log::error!("lab station: the scenario's .env does not parse: {error}");
        ExitCode::FAILURE
    })?;
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
        connect_limit: scenario.connect_limit,
        reconnect: POLICY,
        lab_ap: Some(LabAp { link: ap_link.clone(), network: scenario.ap }),
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

    match scenario.expect {
        Expect::Joins => {
            let mut failed_before_joining = false;
            wait_for(sched, &mut net, "port and IP verdict from the station", |_, net| {
                failed_before_joining |= matches!(shared.state(), StationState::Failed { .. });
                net.links() >= 1 && shared.verdict().is_some()
            })?;
            if failed_before_joining {
                log::error!("lab station: {}: the station backed off before it joined", scenario.name);
                return Err(ExitCode::FAILURE);
            }
            if shared.verdict() != Some(true) || !shared.state().is_connected() {
                log::error!(
                    "lab station: {}: the link failed its IP checks; the state is {}",
                    scenario.name,
                    shared.state()
                );
                return Err(ExitCode::FAILURE);
            }
            log::info!(
                "lab station: {}: {}, leased an address and passed the IP checks",
                scenario.name,
                shared.state()
            );
        },
        Expect::Fails => {
            wait_for(sched, &mut net, "a failed attempt", |_, _| {
                matches!(shared.state(), StationState::Failed { .. })
            })?;
            if net.links() != 0 {
                log::error!("lab station: {}: a port was offered", scenario.name);
                return Err(ExitCode::FAILURE);
            }
            log::info!("lab station: {}: the station did not join, as expected: {}", scenario.name, shared.state());
        },
    }
    shared.request_stop();
    wait_for(sched, &mut net, "stop", |sched, _| sched.info(id).is_none())?;
    if shared.state() != StationState::Stopped || shared.next_port_event().is_some() {
        log::error!("lab station: after the stop the state is {}", shared.state());
        return Err(ExitCode::FAILURE);
    }
    log::info!("lab station: {}: stopped", scenario.name);
    Ok(())
}

fn drive(sched: &mut LabScheduler) -> Result<(), ExitCode> {
    let credentials = Credentials::from_env(lab_env().as_bytes()).map_err(|error| {
        log::error!("lab station: the lab credentials are invalid: {error}");
        ExitCode::FAILURE
    })?;
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
        lab_ap: Some(LabAp { link: ap_link.clone(), network: LabNetwork::PSK }),
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
