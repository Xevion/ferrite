//! A minimal access point for the lab, in the place of hostapd: mac80211 in AP mode leaves
//! authentication, association and the 4-way handshake to userspace. On the AP's own nl80211 socket it
//! starts beaconing (with an RSN element for a secured network), installs the group key, registers for
//! authentication and (re)association requests, answers them with `NL80211_CMD_FRAME`, admits the
//! station with `NEW_STATION`, runs the authenticator side of the 4-way handshake over the control port,
//! then installs the pairwise key and authorizes the station. It runs on a kthread of its own, as hostapd
//! is a process of its own, so the station's blocking client and the AP make progress side by side.
//!
//! It serves one network, as [`LabNetwork`] describes it: open, WPA2-PSK, WPA3-SAE (with or without
//! hash-to-element) or both together as a transition network. For SAE it answers a commit with its own
//! commit and confirm, as hostapd does, and checks the station's confirm; a hidden network answers only
//! the probe requests that name it.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ffi::{c_int, c_void};
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;

use ferrite_lkpi::sched::SchedHandle;
use ferrite_nl80211::client::{
    AKM_PSK, AKM_SAE, ApConfig, CIPHER_AES_CMAC, CIPHER_CCMP, Client, KeyParams, StationParams,
};
use ferrite_nl80211::mlme::Event;
use ferrite_nl80211::nl80211::{Frequency, IfIndex, MacAddr, elements};
use ferrite_wpa::Rng as _;
use ferrite_wpa::authenticator::{AuthStep, Authenticator, Params};
use ferrite_wpa::eapol::ETH_P_PAE;
use ferrite_wpa::kdf::{Passphrase, Pmk, Ssid};
use ferrite_wpa::rsn::{Akm, CAP_MFPC, CAP_MFPR, ELEMENT_RSN, ELEMENT_RSNX, Gtk, Igtk, RSNX_H2E_ELEMENT, Rsn, Suite};
use ferrite_wpa::sae::{self, AuthFrame, Method, Pt, Pwe, Sae};

use crate::connect::{KernelRng, wpa_mac};
use crate::scan::{KernelSocket, ScanError};
use crate::station::{LabNetwork, LabSecurity};

/// Frame control values (type and subtype) of the management frames the AP handles.
const FC_ASSOC_REQ: u16 = 0x0000;
const FC_ASSOC_RESP: u16 = 0x0010;
const FC_REASSOC_REQ: u16 = 0x0020;
const FC_REASSOC_RESP: u16 = 0x0030;
const FC_PROBE_REQ: u16 = 0x0040;
const FC_PROBE_RESP: u16 = 0x0050;
const FC_DISASSOC: u16 = 0x00a0;
const FC_AUTH: u16 = 0x00b0;
const FC_DEAUTH: u16 = 0x00c0;
const HEADER_LEN: usize = 24;
const ELEMENT_SSID: u8 = 0;
const ELEMENT_RATES: u8 = 1;
const ELEMENT_DS_PARAMETER: u8 = 3;
const ELEMENT_EXT_RATES: u8 = 50;
const RATES: [u8; 4] = [0x82, 0x84, 0x8b, 0x96];
const GTK_INDEX: u8 = 1;
const IGTK_INDEX: u8 = 4;
const BEACON_INTERVAL: u16 = 10;
const CHANNEL: u8 = 1;
const POLL: Duration = Duration::from_millis(100);

/// Set by the AP thread as the handshake lands, for the init task's checks.
pub static PORT_OPEN: AtomicBool = AtomicBool::new(false);
pub static GROUP_KEY_INSTALLED: AtomicBool = AtomicBool::new(false);
/// Set by a test to make the AP deauthenticate its station at its next look.
static DEAUTH_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Ask the lab AP to deauthenticate its station, as an access point that drops a client would.
pub fn request_deauth() {
    DEAUTH_REQUESTED.store(true, Ordering::Release);
}

struct Ap {
    client: Client<KernelSocket>,
    sched: SchedHandle,
    ifindex: IfIndex,
    mac: MacAddr,
    network: LabNetwork,
    /// The PMK of the passphrase, for a station that joins with WPA2-PSK.
    psk: Option<Pmk>,
    ssid: Ssid,
    password: Option<Passphrase>,
    /// The beacon's RSN element, and the RSNX element after it when hash-to-element is offered.
    rsn: Vec<u8>,
    rsnx: Option<Vec<u8>>,
    gtk: Gtk,
    igtk: Option<Igtk>,
    station: Option<Station>,
    /// An SAE exchange in progress with a station, and the PMK of the last one that completed.
    sae: Option<SaeExchange>,
    sae_done: Option<(MacAddr, Pmk)>,
    /// A station the AP has sent a deauthentication to: its removal waits for the frame's transmit status.
    deauthenticated: Option<Deauthenticated>,
}

struct SaeExchange {
    station: MacAddr,
    sae: Sae,
}

struct Deauthenticated {
    mac: MacAddr,
    cookie: u64,
}

struct Station {
    mac: MacAddr,
    /// `None` on an open network, which has no handshake.
    authenticator: Option<Authenticator>,
    /// The association response's cookie, whose acknowledged transmit status starts the handshake.
    assoc_cookie: Option<u64>,
}

impl LabNetwork {
    /// The RSN element the beacon carries, and the AKM suites it names for nl80211.
    fn rsn(&self) -> Option<(Rsn, Vec<u32>)> {
        let sae_rsn = |akms: &[Suite], capabilities| Rsn {
            akms: akms.to_vec(),
            capabilities,
            group_mgmt: Some(Suite::BIP_CMAC_128),
            ..Rsn::psk_ccmp()
        };
        match self.security {
            LabSecurity::Open => None,
            LabSecurity::Psk => Some((Rsn::psk_ccmp(), alloc::vec![AKM_PSK])),
            LabSecurity::Sae { .. } => Some((sae_rsn(&[Suite::SAE], CAP_MFPR | CAP_MFPC), alloc::vec![AKM_SAE])),
            LabSecurity::Transition => {
                Some((sae_rsn(&[Suite::PSK, Suite::SAE], CAP_MFPC), alloc::vec![AKM_PSK, AKM_SAE]))
            },
        }
    }

    const fn label(&self) -> &'static str {
        match self.security {
            LabSecurity::Open => "open",
            LabSecurity::Psk => "WPA2-PSK",
            LabSecurity::Sae { .. } => "WPA3-SAE",
            LabSecurity::Transition => "WPA2/WPA3 transition",
        }
    }

    const fn offers_h2e(&self) -> bool {
        matches!(self.security, LabSecurity::Sae { h2e: true } | LabSecurity::Transition)
    }

    const fn offers_sae(&self) -> bool {
        matches!(self.security, LabSecurity::Sae { .. } | LabSecurity::Transition)
    }
}

/// Start beaconing `network` on 2412 MHz from `ifindex`, install the group key and register for the
/// frames hostapd handles. The mlme group carries the transmit status of sent frames.
pub fn start(
    mut client: Client<KernelSocket>,
    sched: SchedHandle,
    ifindex: IfIndex,
    mac: MacAddr,
    network: &LabNetwork,
) -> Result<ApTask, ScanError> {
    client.join_mlme()?;
    let (ssid, password) = (Ssid::new(network.ssid).ok(), Passphrase::new(network.passphrase).ok());
    let Some(ssid) = ssid else { return Err(ScanError::Thread { code: -22 }) };
    let secured = network.rsn();
    let mut tail = secured.as_ref().map(|(rsn, _)| rsn.element());
    let rsnx = network.offers_h2e().then(|| RSNX_H2E_ELEMENT.to_vec());
    if let (Some(tail), Some(rsnx)) = (tail.as_mut(), &rsnx) {
        tail.extend_from_slice(rsnx);
    }
    let akms = secured.as_ref().map_or_default(|(_, akms)| akms.clone());
    let ap = ApConfig {
        ssid: network.ssid,
        rsn: tail.as_deref(),
        akms: &akms,
        hidden: network.hidden,
        frequency: Frequency(2412),
        beacon_interval: BEACON_INTERVAL,
        channel: CHANNEL,
    };
    client.start_ap(ifindex, mac, &ap)?;
    let mut key = alloc::vec![0; 16];
    KernelRng.fill(&mut key);
    let gtk = Gtk { index: GTK_INDEX, key };
    let mut igtk = None;
    if secured.is_some() {
        client.new_key(
            ifindex,
            &KeyParams { peer: None, index: GTK_INDEX, cipher: CIPHER_CCMP, data: &gtk.key, seq: None },
        )?;
        client.set_default_multicast_key(ifindex, GTK_INDEX)?;
        GROUP_KEY_INSTALLED.store(true, Ordering::Release);
        if network.offers_sae() {
            let mut key = [0; 16];
            KernelRng.fill(&mut key);
            let igtk_key = Igtk { index: u16::from(IGTK_INDEX), ipn: [0; 6], key };
            client.new_key(
                ifindex,
                &KeyParams { peer: None, index: IGTK_INDEX, cipher: CIPHER_AES_CMAC, data: &igtk_key.key, seq: None },
            )?;
            client.set_default_mgmt_key(ifindex, IGTK_INDEX)?;
            igtk = Some(igtk_key);
        }
    }
    let mut frame_types = alloc::vec![FC_AUTH, FC_ASSOC_REQ, FC_REASSOC_REQ, FC_DEAUTH, FC_DISASSOC];
    if network.hidden {
        frame_types.push(FC_PROBE_REQ);
    }
    for frame_type in frame_types {
        client.register_frame(ifindex, frame_type)?;
    }
    log::info!(
        "lab AP: {mac} beacons the {} network \"{}\" on 2412 MHz{}{}",
        network.label(),
        ferrite_nl80211::nl80211::SsidDisplay(network.ssid),
        if secured.is_some() {
            alloc::format!("; group key {GTK_INDEX} installed")
        } else {
            alloc::string::String::new()
        },
        if network.hidden { " (hidden)" } else { "" }
    );
    let psk = matches!(network.security, LabSecurity::Psk | LabSecurity::Transition)
        .then(|| password.as_ref().map(|password| Pmk::from_passphrase(password, &ssid)))
        .flatten();
    let state = Box::new(Ap {
        client,
        sched,
        ifindex,
        mac,
        network: *network,
        psk,
        ssid,
        password,
        rsn: secured.map_or_default(|(rsn, _)| rsn.element()),
        rsnx,
        gtk,
        igtk,
        station: None,
        sae: None,
        sae_done: None,
        deauthenticated: None,
    });
    let arg = Box::into_raw(state);
    // SAFETY: called on a task with kthreadd running; the format takes no arguments.
    let task = unsafe { crate::kthread_create_on_node(thread, arg.cast(), crate::NUMA_NO_NODE, c"lab-ap".as_ptr()) };
    if task.is_null() || task as usize > usize::MAX - crate::MAX_ERRNO {
        // SAFETY: the thread was not created, so nothing else holds the box.
        drop(unsafe { Box::from_raw(arg) });
        return Err(ScanError::Thread { code: task as isize });
    }
    // SAFETY: a live task_struct kthread_create returned.
    unsafe { crate::wake_up_process(task) };
    Ok(ApTask(task))
}

/// The AP's thread; stopping it closes the AP's sockets.
pub struct ApTask(*mut c_void);

impl ApTask {
    pub fn stop(self) {
        // SAFETY: the thread runs until told to stop, so the task_struct is live.
        unsafe { crate::kthread_stop(self.0) };
    }
}

unsafe extern "C" fn thread(arg: *mut c_void) -> c_int {
    // SAFETY: `start` passed a leaked `Ap` that only this thread uses.
    let mut ap = unsafe { Box::from_raw(arg.cast::<Ap>()) };
    // SAFETY: runs on a kthread.
    while !unsafe { crate::kthread_should_stop() } {
        if DEAUTH_REQUESTED.swap(false, Ordering::AcqRel)
            && let Err(error) = ap.deauth()
        {
            log::error!("lab AP: {error}");
        }
        let sched = ap.sched.clone();
        let clock = || sched.now();
        match ap.client.next_event(clock() + POLL, &clock) {
            Ok(Some(event)) if event.ifindex() == Some(ap.ifindex) => {
                if let Err(error) = ap.event(event) {
                    log::error!("lab AP: {error}");
                }
            },
            Ok(_) => {},
            Err(error) => {
                // SAFETY: runs on a kthread.
                if !unsafe { crate::kthread_should_stop() } {
                    log::error!("lab AP: {error}");
                }
            },
        }
    }
    0
}

fn header(fc: u16, da: MacAddr, ap: MacAddr) -> Vec<u8> {
    let mut frame = Vec::with_capacity(64);
    frame.extend_from_slice(&fc.to_le_bytes());
    frame.extend_from_slice(&[0, 0]);
    frame.extend_from_slice(&da.0);
    frame.extend_from_slice(&ap.0);
    frame.extend_from_slice(&ap.0);
    frame.extend_from_slice(&[0, 0]);
    frame
}

/// Reason code 2: the previous authentication is no longer valid.
const REASON_PREV_AUTH_INVALID: u16 = 2;

type ClientResult = Result<(), ferrite_nl80211::client::ClientError>;

impl Ap {
    fn deauth(&mut self) -> ClientResult {
        let Some(station) = self.station.take() else { return Ok(()) };
        let mut frame = header(FC_DEAUTH, station.mac, self.mac);
        frame.extend_from_slice(&REASON_PREV_AUTH_INVALID.to_le_bytes());
        let cookie = self.client.send_frame(self.ifindex, &frame)?;
        self.deauthenticated = Some(Deauthenticated { mac: station.mac, cookie });
        PORT_OPEN.store(false, Ordering::Release);
        log::info!("lab AP: deauthenticated {}", station.mac);
        Ok(())
    }

    /// Drop `mac` from the kernel's station table, as hostapd does when a station leaves or is removed:
    /// without it a later association of the same address finds the entry and `NEW_STATION` fails with
    /// `EEXIST`.
    fn remove_station(&mut self, mac: MacAddr) -> ClientResult {
        self.client.del_station(self.ifindex, mac)?;
        log::info!("lab AP: station {mac} removed");
        Ok(())
    }

    fn event(&mut self, event: Event) -> ClientResult {
        match event {
            Event::Frame { frame, .. } if frame.len() >= HEADER_LEN => {
                let fc = u16::from_le_bytes([frame[0], frame[1]]) & 0x00fc;
                let mut source = MacAddr::default();
                source.0.copy_from_slice(&frame[10..16]);
                let body = &frame[HEADER_LEN..];
                match fc {
                    FC_AUTH => self.auth(source, body),
                    FC_ASSOC_REQ => self.assoc(source, body.get(4..).unwrap_or_default(), body, false),
                    FC_REASSOC_REQ => self.assoc(source, body.get(10..).unwrap_or_default(), body, true),
                    FC_PROBE_REQ => self.probe(source, body),
                    _ => {
                        log::info!("lab AP: {source} left (frame control {fc:#06x})");
                        PORT_OPEN.store(false, Ordering::Release);
                        match self.station.take() {
                            Some(station) if station.mac == source => self.remove_station(source),
                            other => {
                                self.station = other;
                                Ok(())
                            },
                        }
                    },
                }
            },
            Event::FrameTxStatus { cookie, .. }
                if self.deauthenticated.as_ref().is_some_and(|gone| gone.cookie == cookie) =>
            {
                let Some(gone) = self.deauthenticated.take() else { return Ok(()) };
                self.remove_station(gone.mac)
            },
            Event::FrameTxStatus { cookie, ack, .. } => {
                let Some(station) = self.station.as_mut().filter(|s| s.assoc_cookie == Some(cookie)) else {
                    return Ok(());
                };
                station.assoc_cookie = None;
                if !ack {
                    log::warn!("lab AP: the association response to {} was not acknowledged", station.mac);
                    return Ok(());
                }
                let Some(authenticator) = station.authenticator.as_mut() else { return Ok(()) };
                let message_1 = authenticator.start(&mut KernelRng);
                let dest = station.mac;
                self.client.send_eapol(self.ifindex, dest, &message_1)?;
                log::info!("lab AP: 4-way message 1 sent to {dest}");
                Ok(())
            },
            Event::ControlPortFrame { source, ethertype: ETH_P_PAE, frame, .. } => self.eapol(source, &frame),
            _ => Ok(()),
        }
    }

    /// Answer a directed probe request for a hidden network with the network's real name; a broadcast
    /// one gets nothing, as hostapd's `ignore_broadcast_ssid` has it.
    fn probe(&mut self, source: MacAddr, body: &[u8]) -> ClientResult {
        let asked = elements(body).find(|(id, _)| *id == ELEMENT_SSID).map(|(_, ssid)| ssid);
        if !self.network.hidden || asked != Some(self.network.ssid) {
            return Ok(());
        }
        let mut frame = header(FC_PROBE_RESP, source, self.mac);
        frame.extend_from_slice(&[0; 8]);
        frame.extend_from_slice(&BEACON_INTERVAL.to_le_bytes());
        frame.extend_from_slice(&(if self.rsn.is_empty() { 0x0001_u16 } else { 0x0011 }).to_le_bytes());
        frame.extend_from_slice(&[ELEMENT_SSID, self.network.ssid.len() as u8]);
        frame.extend_from_slice(self.network.ssid);
        frame.extend_from_slice(&[ELEMENT_RATES, RATES.len() as u8]);
        frame.extend_from_slice(&RATES);
        frame.extend_from_slice(&[ELEMENT_DS_PARAMETER, 1, CHANNEL]);
        frame.extend_from_slice(&self.rsn);
        frame.extend_from_slice(self.rsnx.as_deref().unwrap_or_default());
        self.client.send_frame(self.ifindex, &frame)?;
        log::info!("lab AP: probe response with the hidden name sent to {source}");
        Ok(())
    }

    fn send_auth(&mut self, dest: MacAddr, transaction: u16, status: u16, body: &[u8]) -> ClientResult {
        let mut frame = header(FC_AUTH, dest, self.mac);
        frame.extend_from_slice(&sae::AUTH_ALGORITHM.to_le_bytes());
        frame.extend_from_slice(&transaction.to_le_bytes());
        frame.extend_from_slice(&status.to_le_bytes());
        frame.extend_from_slice(body);
        self.client.send_frame(self.ifindex, &frame).map(drop)
    }

    fn auth(&mut self, source: MacAddr, body: &[u8]) -> ClientResult {
        let Some(frame) = AuthFrame::parse(body) else { return Ok(()) };
        match frame.algorithm {
            0 if frame.transaction == 1 && !matches!(self.network.security, LabSecurity::Sae { .. }) => {
                let mut reply = header(FC_AUTH, source, self.mac);
                reply.extend_from_slice(&[0, 0, 2, 0, 0, 0]);
                self.client.send_frame(self.ifindex, &reply)?;
                log::info!("lab AP: open system authentication of {source} answered with status 0");
                Ok(())
            },
            sae::AUTH_ALGORITHM if self.network.offers_sae() => self.sae_auth(source, &frame),
            _ => Ok(()),
        }
    }

    /// The access point's half of SAE: a commit from the station is answered with our commit and our
    /// confirm, and the station's confirm completes the exchange.
    fn sae_auth(&mut self, source: MacAddr, frame: &AuthFrame<'_>) -> ClientResult {
        match (frame.transaction, frame.status) {
            (1, status) => {
                let method = match status {
                    sae::STATUS_SUCCESS => Method::HuntAndPeck,
                    sae::STATUS_H2E if self.network.offers_h2e() => Method::HashToElement,
                    _ => {
                        log::warn!("lab AP: SAE commit of {source} with status {status} is not one this network takes");
                        return Ok(());
                    },
                };
                let Some(password) = &self.password else { return Ok(()) };
                let (own, peer) = (wpa_mac(self.mac), wpa_mac(source));
                let pwe: Pwe = match method {
                    Method::HuntAndPeck => Pwe::hunt_and_peck(password, own, peer),
                    Method::HashToElement => Pt::derive(&self.ssid, password, None).pwe(own, peer),
                };
                let mut exchange = Sae::new(method, pwe, &mut KernelRng);
                if let Err(error) = exchange.process_commit(frame.body) {
                    log::warn!("lab AP: SAE commit of {source} refused: {error}");
                    return Ok(());
                }
                let Ok(confirm) = exchange.confirm_body() else { return Ok(()) };
                self.send_auth(source, 1, method.commit_status(), &exchange.commit_body(None))?;
                self.send_auth(source, 2, sae::STATUS_SUCCESS, &confirm)?;
                log::info!("lab AP: SAE commit of {source} accepted ({method:?}); commit and confirm sent");
                self.sae = Some(SaeExchange { station: source, sae: exchange });
                Ok(())
            },
            (2, sae::STATUS_SUCCESS) => {
                let Some(exchange) = self.sae.take().filter(|e| e.station == source) else { return Ok(()) };
                match exchange.sae.check_confirm(frame.body) {
                    Ok(()) => {
                        if let Some(pmk) = exchange.sae.pmk() {
                            self.sae_done = Some((source, pmk.clone()));
                        }
                        log::info!("lab AP: SAE with {source} complete; its confirm verified");
                    },
                    Err(error) => log::warn!("lab AP: SAE confirm of {source} refused: {error}"),
                }
                Ok(())
            },
            _ => Ok(()),
        }
    }

    fn assoc(&mut self, source: MacAddr, ies: &[u8], body: &[u8], reassoc: bool) -> ClientResult {
        let (capability, listen_interval) = match body {
            [c0, c1, l0, l1, ..] => (u16::from_le_bytes([*c0, *c1]), u16::from_le_bytes([*l0, *l1])),
            _ => return Ok(()),
        };
        let mut rates = Vec::new();
        let mut sta_rsn = None;
        let mut sta_rsnx = None;
        for (id, data) in elements(ies) {
            let whole = |id: u8| {
                let mut element = alloc::vec![id, data.len() as u8];
                element.extend_from_slice(data);
                element
            };
            match id {
                ELEMENT_RATES | ELEMENT_EXT_RATES => rates.extend_from_slice(data),
                ELEMENT_RSN => sta_rsn = Some(whole(ELEMENT_RSN)),
                ELEMENT_RSNX => sta_rsnx = Some(whole(ELEMENT_RSNX)),
                _ => {},
            }
        }
        let akm = match sta_rsn.as_deref().and_then(|rsn| Rsn::parse(rsn.get(2..)?).ok()) {
            _ if self.rsn.is_empty() => None,
            Some(rsn) if rsn.akms.contains(&Suite::SAE) => Some(Akm::Sae),
            Some(_) => Some(Akm::Psk),
            None => {
                log::warn!("lab AP: {source} associated without an RSN element; ignored");
                return Ok(());
            },
        };
        let pmk = match akm {
            None => None,
            Some(Akm::Psk) => {
                let Some(psk) = &self.psk else {
                    log::warn!("lab AP: {source} wants WPA2-PSK, which this network does not offer; ignored");
                    return Ok(());
                };
                Some(psk.clone())
            },
            Some(Akm::Sae) => {
                let Some((_, pmk)) = self.sae_done.take().filter(|(station, _)| *station == source) else {
                    log::warn!("lab AP: {source} associated for SAE without a completed exchange; ignored");
                    return Ok(());
                };
                Some(pmk)
            },
        };
        self.client.new_station(
            self.ifindex,
            &StationParams {
                mac: source,
                aid: 1,
                listen_interval,
                capability,
                supported_rates: &rates,
                mfp: akm == Some(Akm::Sae),
            },
        )?;
        let mut frame = header(if reassoc { FC_REASSOC_RESP } else { FC_ASSOC_RESP }, source, self.mac);
        frame.extend_from_slice(&0x0011_u16.to_le_bytes());
        frame.extend_from_slice(&0_u16.to_le_bytes());
        frame.extend_from_slice(&(1_u16 | 0xc000).to_le_bytes());
        frame.extend_from_slice(&[ELEMENT_RATES, RATES.len() as u8]);
        frame.extend_from_slice(&RATES);
        let cookie = self.client.send_frame(self.ifindex, &frame)?;
        // An open network has no handshake: the station is admitted and authorized with the response.
        let (Some(akm), Some(pmk), Some(sta_rsn)) = (akm, pmk, sta_rsn) else {
            self.client.authorize(self.ifindex, source)?;
            self.station = Some(Station { mac: source, authenticator: None, assoc_cookie: None });
            PORT_OPEN.store(true, Ordering::Release);
            log::info!("lab AP: {source} associated with AID 1 on an open network; station added and authorized");
            return Ok(());
        };
        let authenticator = Authenticator::new(Params {
            akm,
            pmk,
            aa: wpa_mac(self.mac),
            spa: wpa_mac(source),
            own_rsn: &self.rsn,
            own_rsnx: self.rsnx.as_deref(),
            sta_rsn: &sta_rsn,
            sta_rsnx: sta_rsnx.as_deref(),
            gtk: self.gtk.clone(),
            igtk: if akm == Akm::Sae { self.igtk.clone() } else { None },
        });
        self.station = Some(Station { mac: source, authenticator: Some(authenticator), assoc_cookie: Some(cookie) });
        log::info!("lab AP: {source} associated with AID 1; station added");
        Ok(())
    }

    fn eapol(&mut self, source: MacAddr, frame: &[u8]) -> ClientResult {
        let Some(authenticator) =
            self.station.as_mut().filter(|s| s.mac == source).and_then(|s| s.authenticator.as_mut())
        else {
            return Ok(());
        };
        match authenticator.handle(frame) {
            Ok(AuthStep::Reply(message_3)) => {
                self.client.send_eapol(self.ifindex, source, &message_3)?;
                log::info!("lab AP: 4-way message 2 verified, message 3 sent");
            },
            Ok(AuthStep::Complete { tk }) => {
                self.client.new_key(
                    self.ifindex,
                    &KeyParams { peer: Some(source), index: 0, cipher: CIPHER_CCMP, data: &tk, seq: None },
                )?;
                self.client.authorize(self.ifindex, source)?;
                PORT_OPEN.store(true, Ordering::Release);
                log::info!("lab AP: 4-way complete with {source}; pairwise key installed, station authorized");
            },
            Ok(AuthStep::GroupComplete) => log::info!("lab AP: group key handshake complete with {source}"),
            Err(error) => log::warn!("lab AP: dropped an EAPOL frame from {source}: {error}"),
        }
        Ok(())
    }
}
