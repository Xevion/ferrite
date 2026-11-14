//! The values checked: the plain sections of the file into the validated types the apps use, by the
//! same constructors they use, so a value is accepted here exactly when it would be there.

use alloc::vec::Vec;
use core::num::NonZeroU64;

use ferrite_net::notify::{self, Event, Events, Format, Host};
use ferrite_net::sntp::MAX_SERVERS;
use ferrite_wpa::env::{Allow, Auth, Credentials, Network};
use ferrite_wpa::kdf::{Passphrase, Ssid};

use crate::error::{Path, Problem};
use crate::run::{RunGoal, parse_duration};
use crate::schema::{self, NotifyEvent, NotifyFormat, Security};
use crate::time::{TimeSettings, zone_named};

/// The most networks a file may list. The station tries a few at a time, so more is a mistake.
pub const MAX_NETWORKS: usize = 16;

/// A value that is the wrong one, and where it is.
#[derive(Debug)]
pub struct Invalid {
    pub path: Path,
    pub problem: Problem,
}

const fn invalid(path: Path, problem: Problem) -> Invalid {
    Invalid { path, problem }
}

pub fn credentials(wifi: &schema::Wifi) -> Result<Credentials, Invalid> {
    let section = Path::default().key("wifi");
    let networks = section.clone().key("networks");
    if wifi.networks.len() > MAX_NETWORKS {
        return Err(invalid(networks.index(MAX_NETWORKS), Problem::TooManyNetworks { max: MAX_NETWORKS }));
    }
    let all = wifi
        .networks
        .iter()
        .enumerate()
        .map(|(i, network)| network_of(network, &networks.clone().index(i)))
        .collect::<Result<Vec<_>, _>>()?;
    Credentials::new(all).ok_or_else(|| invalid(section, Problem::NoNetworks))
}

fn network_of(network: &schema::Network, at: &Path) -> Result<Network, Invalid> {
    let ssid = Ssid::new(network.ssid.as_bytes()).map_err(|_| invalid(at.clone().key("ssid"), Problem::Ssid))?;
    let auth = match (network.security, &network.passphrase) {
        (Security::Open, None) => Auth::Open,
        (Security::Open, Some(_)) => return Err(invalid(at.clone().key("passphrase"), Problem::OpenWithPassphrase)),
        (_, None) => return Err(invalid(at.clone().key("ssid"), Problem::PassphraseMissing)),
        (security, Some(passphrase)) => {
            let passphrase = Passphrase::new(passphrase.expose().as_bytes())
                .map_err(|_| invalid(at.clone().key("passphrase"), Problem::Passphrase))?;
            let allow = match security {
                Security::Wpa2 => Allow::PskOnly,
                Security::Wpa3 => Allow::SaeOnly,
                Security::Auto | Security::Open => Allow::Any,
            };
            Auth::Personal { passphrase, allow }
        },
    };
    Ok(Network { ssid, hidden: network.hidden, auth })
}

pub fn notify_config(section: &schema::Notify) -> Result<notify::Config, Invalid> {
    let at = Path::default().key("notify");
    let events = match &section.events {
        None => Events::ALL,
        Some(list) if list.is_empty() => return Err(invalid(at.key("events"), Problem::NoEvents)),
        Some(list) => Events::of(&list.iter().map(|event| event_of(*event)).collect::<Vec<_>>()),
    };
    let format = match section.format {
        NotifyFormat::Text => Format::Text,
        NotifyFormat::Json => Format::Json,
    };
    notify::Config::new(section.url.expose(), format, events)
        .map_err(|problem| invalid(at.key("url"), Problem::Url { problem }))
}

const fn event_of(event: NotifyEvent) -> Event {
    match event {
        NotifyEvent::Pass => Event::Pass,
        NotifyEvent::Fault => Event::Fault,
        NotifyEvent::End => Event::End,
    }
}

pub fn run_goal(section: &schema::Run) -> Result<RunGoal, Invalid> {
    let at = Path::default().key("run");
    let passes = section
        .passes
        .map(|n| NonZeroU64::new(n).ok_or_else(|| invalid(at.clone().key("passes"), Problem::Passes)))
        .transpose()?;
    let time = section
        .time
        .as_deref()
        .map(|text| parse_duration(text).ok_or_else(|| invalid(at.clone().key("time"), Problem::Time)))
        .transpose()?;
    Ok(RunGoal { passes, time, stop_on_fault: section.stop_on_fault })
}

pub fn time_settings(section: &schema::Time) -> Result<TimeSettings, Invalid> {
    let at = Path::default().key("time");
    let list = at.clone().key("servers");
    let named = section.servers.as_deref().unwrap_or_default();
    if named.len() > MAX_SERVERS {
        return Err(invalid(list.index(MAX_SERVERS), Problem::TooManyNtpServers { max: MAX_SERVERS }));
    }
    let servers = named
        .iter()
        .enumerate()
        .map(|(i, text)| Host::parse(text).ok_or_else(|| invalid(list.clone().index(i), Problem::NtpServer)))
        .collect::<Result<Vec<_>, _>>()?;
    let zone = match &section.zone {
        None => jiff::tz::TimeZone::UTC,
        Some(name) => zone_named(name).ok_or_else(|| invalid(at.key("zone"), Problem::Zone))?,
    };
    Ok(TimeSettings { ntp: section.ntp, servers, zone, clock: section.clock })
}
