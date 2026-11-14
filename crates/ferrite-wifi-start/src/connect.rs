//! A connection as iwd makes one, for an open network, WPA2-PSK and WPA3-SAE.
//!
//! Open and WPA2-PSK use `NL80211_CMD_CONNECT`: cfg80211's SME authenticates and associates. For SAE the
//! SME cannot do the work, so the exchange runs here as `wpa_supplicant` runs it on mac80211: SAE's
//! commit and confirm go out as `NL80211_CMD_AUTHENTICATE` and the access point's come back as events
//! on the mlme group, then `NL80211_CMD_ASSOCIATE` carries the PMKID, with protected management frames
//! required. (`NL80211_CMD_EXTERNAL_AUTH` is for drivers whose firmware has its own SME, which none of
//! mac80211's has.) After that the 4-way handshake runs in Rust (`ferrite-wpa`) over the control port,
//! and the keys and the open port go back through nl80211. An open network has no handshake: the
//! connect event is the end of it.

use alloc::vec::Vec;
use core::ffi::c_void;
use core::time::Duration;

use ferrite_hal::Instant;
use ferrite_nl80211::client::{
    AssociateParams, CIPHER_AES_CMAC, CIPHER_CCMP, Client, ClientError, ConnectParams, KeyParams,
};
use ferrite_nl80211::mlme::{Event, ReasonCode, StatusCode};
use ferrite_nl80211::nl80211::{Bss, IfIndex, MacAddr};
use ferrite_wifi::select::Join;
use ferrite_wpa::eapol::ETH_P_PAE;
use ferrite_wpa::env::{Auth, Network};
use ferrite_wpa::kdf::{Passphrase, Pmk, Ssid};
use ferrite_wpa::rsn::{Akm, RSNX_H2E_ELEMENT, Rsn};
use ferrite_wpa::sae::{self, AuthFrame, Pt, Pwe, Sae, SaeError};
use ferrite_wpa::supplicant::{HandshakeError, Install, Params, Supplicant};
use snafu::Snafu;

use crate::scan::KernelSocket;

unsafe extern "C" {
    fn get_random_bytes(buf: *mut c_void, len: usize);
}

/// Nonces from the kernel's random pool, which the platform seeded from its entropy source.
pub struct KernelRng;

impl ferrite_wpa::Rng for KernelRng {
    fn fill(&mut self, buf: &mut [u8]) {
        // SAFETY: `buf` is writable for its length; the call only fills it.
        unsafe { get_random_bytes(buf.as_mut_ptr().cast(), buf.len()) };
    }
}

pub const fn wpa_mac(mac: MacAddr) -> ferrite_wpa::MacAddr {
    ferrite_wpa::MacAddr(mac.0)
}

#[derive(Debug, Snafu)]
pub enum ConnectError {
    #[snafu(display("{step}: {source}"))]
    Nl80211 { step: &'static str, source: ClientError },
    #[snafu(display("the AP rejected the connection: status {status}{}", if *timed_out { ", timed out" } else { "" }))]
    Rejected { status: StatusCode, timed_out: bool },
    #[snafu(display("the AP rejected SAE: status {status}"))]
    SaeRejected { status: StatusCode },
    #[snafu(display("SAE: {source}"))]
    Sae { source: SaeError },
    #[snafu(display("disconnected during {step}: reason {}{}", reason.map_or_else(|| alloc::string::String::from("none"), |r| alloc::format!("{r}")), if *by_ap { ", by the AP" } else { "" }))]
    Disconnected { step: &'static str, reason: Option<ReasonCode>, by_ap: bool },
    #[snafu(display("no {step} within {} ms", limit.as_millis()))]
    Expired { step: &'static str, limit: Duration },
    #[snafu(display("the BSS advertises no RSN element"))]
    NoRsn,
    #[snafu(display("the BSS does not offer what the network allows"))]
    Unsupported,
}

const HOLD_SLICE: Duration = Duration::from_millis(100);
/// How long an abort waits for the disconnect event of the connection it ends.
const ABORT_WAIT: Duration = Duration::from_millis(500);
/// The 802.11 header of an authentication frame as `NL80211_CMD_AUTHENTICATE` delivers it.
const AUTH_HEADER_LEN: usize = 24;

fn step(step: &'static str) -> impl FnOnce(ClientError) -> ConnectError {
    move |source| ConnectError::Nl80211 { step, source }
}

/// What a connection needs to authenticate: nothing, the PMK of a passphrase, or the password.
pub enum Secret<'a> {
    Open,
    Psk(Pmk),
    Sae { ssid: &'a Ssid, password: &'a Passphrase, method: sae::Method },
}

/// The secret `join` needs for `network`: the PMK of its passphrase for WPA2-PSK (derived here, which
/// takes a few milliseconds), its password for SAE, nothing for an open network. `None` when `join` is
/// not one the network's authentication can do.
pub fn secret_for(network: &Network, join: Join) -> Option<Secret<'_>> {
    match (&network.auth, join) {
        (Auth::Open, Join::Open) => Some(Secret::Open),
        (Auth::Personal { passphrase, .. }, Join::Psk) => {
            Some(Secret::Psk(Pmk::from_passphrase(passphrase, &network.ssid)))
        },
        (Auth::Personal { passphrase, .. }, Join::Sae(method)) => {
            Some(Secret::Sae { ssid: &network.ssid, password: passphrase, method })
        },
        _ => None,
    }
}

/// Tear down what a failed attempt left (best effort: it may already be down) and swallow its
/// disconnect event, so the next attempt on the same client does not take it for its own.
pub fn abort(client: &mut Client<KernelSocket>, ifindex: IfIndex, clock: &dyn Fn() -> Instant) {
    if client.disconnect(ifindex, ReasonCode::DeauthLeaving).is_err() {
        return;
    }
    let deadline = clock() + ABORT_WAIT;
    while let Ok(Some(event)) = client.next_event(deadline, clock) {
        if matches!(event, Event::Disconnect { ifindex: found, .. } if found == ifindex) {
            return;
        }
    }
}

/// A completed connection.
pub struct Connection {
    pub ifindex: IfIndex,
    pub ap: MacAddr,
    /// From the connect request to the connect event.
    pub associated_after: Duration,
    /// From the connect event to the port opening; zero for an open network.
    pub handshake_after: Duration,
    /// `None` for an open network, which has no handshake and no keys.
    supplicant: Option<Supplicant>,
}

/// The supplicant of a secured connection, once the association it is for has started.
struct Pending<'a> {
    akm: Akm,
    pmk: Pmk,
    own_rsn: Vec<u8>,
    own_rsnx: Option<&'a [u8]>,
}

/// Start the connection `secret` calls for: the request that makes the access point answer, and with SAE
/// the whole authentication exchange before it. The pending handshake, if any, and what the next event
/// is the answer to.
fn begin<'a>(
    client: &mut Client<KernelSocket>,
    ifindex: IfIndex,
    own: MacAddr,
    bss: &Bss,
    secret: Secret<'_>,
    (deadline, limit): (Instant, Duration),
    clock: &dyn Fn() -> Instant,
) -> Result<(Option<Pending<'a>>, &'static str), ConnectError> {
    Ok(match secret {
        Secret::Open => {
            if bss.security != ferrite_nl80211::nl80211::Security::Open {
                return Err(ConnectError::Unsupported);
            }
            client
                .connect_network(
                    ifindex,
                    &ConnectParams {
                        ssid: &bss.ssid,
                        bssid: Some(bss.bssid),
                        frequency: Some(bss.frequency),
                        rsn: None,
                    },
                )
                .map_err(step("CONNECT"))?;
            (None, "connect event")
        },
        Secret::Psk(pmk) => {
            let ap_rsn = bss.rsn.as_deref().ok_or(ConnectError::NoRsn)?;
            let offered = Rsn::parse(&ap_rsn[2..]).map_err(|_| ConnectError::Unsupported)?;
            if !offered.supports_psk_ccmp() {
                return Err(ConnectError::Unsupported);
            }
            let own_rsn = Rsn::psk_ccmp().element();
            client
                .connect_network(
                    ifindex,
                    &ConnectParams {
                        ssid: &bss.ssid,
                        bssid: Some(bss.bssid),
                        frequency: Some(bss.frequency),
                        rsn: Some(&own_rsn),
                    },
                )
                .map_err(step("CONNECT"))?;
            (Some(Pending { akm: Akm::Psk, pmk, own_rsn, own_rsnx: None }), "connect event")
        },
        Secret::Sae { ssid, password, method } => {
            let ap_rsn = bss.rsn.as_deref().ok_or(ConnectError::NoRsn)?;
            let offered = Rsn::parse(&ap_rsn[2..]).map_err(|_| ConnectError::Unsupported)?;
            if !offered.supports_sae_ccmp() {
                return Err(ConnectError::Unsupported);
            }
            let (pmk, pmkid) =
                authenticate_sae(client, ifindex, own, bss, (ssid, password, method), (deadline, limit), clock)?;
            let own_rsn = Rsn::sae_ccmp(Some(pmkid)).element();
            let own_rsnx = (method == sae::Method::HashToElement).then_some(&RSNX_H2E_ELEMENT[..]);
            let mut ies = own_rsn.clone();
            ies.extend_from_slice(own_rsnx.unwrap_or_default());
            client
                .associate_sae(
                    ifindex,
                    &AssociateParams { ssid: &bss.ssid, bssid: bss.bssid, frequency: bss.frequency, ies: &ies },
                )
                .map_err(step("ASSOCIATE"))?;
            (Some(Pending { akm: Akm::Sae, pmk, own_rsn, own_rsnx }), "association")
        },
    })
}

/// Connect `ifindex` (with address `own`) to `bss` and, unless the network is open, run the 4-way
/// handshake, within `limit`.
pub fn connect(
    client: &mut Client<KernelSocket>,
    ifindex: IfIndex,
    own: MacAddr,
    bss: &Bss,
    secret: Secret<'_>,
    limit: Duration,
    clock: &dyn Fn() -> Instant,
) -> Result<Connection, ConnectError> {
    client.join_mlme().map_err(step("joining the mlme group"))?;
    let start = clock();
    let deadline = start + limit;
    let (pending, associate) = begin(client, ifindex, own, bss, secret, (deadline, limit), clock)?;
    log::info!("connect: the request was accepted for {} on {} MHz", bss.bssid, bss.frequency.0);

    let mut supplicant = match (&pending, bss.rsn.as_deref()) {
        (Some(pending), Some(ap_rsn)) => Some(Supplicant::new(Params {
            akm: pending.akm,
            pmk: pending.pmk.clone(),
            aa: wpa_mac(bss.bssid),
            spa: wpa_mac(own),
            ap_rsn,
            ap_rsnx: bss.rsnx.as_deref(),
            own_rsn: &pending.own_rsn,
            own_rsnx: pending.own_rsnx,
        })),
        _ => None,
    };
    let mut early: Vec<Vec<u8>> = Vec::new();
    let mut associated_at: Option<Instant> = None;
    loop {
        let what = if associated_at.is_some() { "4-way handshake" } else { associate };
        let Some(event) = client.next_event(deadline, clock).map_err(step("waiting for events"))? else {
            return Err(ConnectError::Expired { step: what, limit });
        };
        if event.ifindex() != Some(ifindex) {
            continue;
        }
        let mut frames: Vec<Vec<u8>> = Vec::new();
        match event {
            Event::Connect { status: StatusCode::Success, .. } => {
                let at = clock();
                associated_at = Some(at);
                log::info!("connect: associated after {} ms (status 0)", (at - start).as_millis());
                if supplicant.is_none() {
                    return Ok(done(ifindex, bss.bssid, start, at, clock, None));
                }
                frames = core::mem::take(&mut early);
            },
            Event::Connect { status, timed_out, .. } => return Err(ConnectError::Rejected { status, timed_out }),
            Event::Disconnect { reason, by_ap, .. } => {
                return Err(ConnectError::Disconnected { step: what, reason, by_ap });
            },
            Event::ControlPortFrame { ethertype: ETH_P_PAE, frame, .. } => {
                if associated_at.is_some() {
                    frames.push(frame);
                } else {
                    early.push(frame);
                }
            },
            other => log::debug!("connect: {other:?}"),
        }
        let (Some(handshake), Some(at)) = (supplicant.as_mut(), associated_at) else { continue };
        for frame in frames {
            if eapol(client, handshake, ifindex, bss.bssid, &frame)? {
                return Ok(done(ifindex, bss.bssid, start, at, clock, supplicant));
            }
        }
    }
}

/// The SAE exchange as the station side of `wpa_supplicant` runs it: our commit, the access point's
/// commit (answered with our confirm) and its confirm. Returns the PMK and the PMKID the association
/// names.
fn authenticate_sae(
    client: &mut Client<KernelSocket>,
    ifindex: IfIndex,
    own: MacAddr,
    bss: &Bss,
    (ssid, password, method): (&Ssid, &Passphrase, sae::Method),
    (deadline, limit): (Instant, Duration),
    clock: &dyn Fn() -> Instant,
) -> Result<(Pmk, [u8; 16]), ConnectError> {
    let (own_addr, ap_addr) = (wpa_mac(own), wpa_mac(bss.bssid));
    let pwe: Pwe = match method {
        sae::Method::HuntAndPeck => Pwe::hunt_and_peck(password, own_addr, ap_addr),
        sae::Method::HashToElement => Pt::derive(ssid, password, None).pwe(own_addr, ap_addr),
    };
    let mut exchange = Sae::new(method, pwe, &mut KernelRng);
    let send = |client: &mut Client<KernelSocket>, transaction: u16, status: u16, body: &[u8]| {
        let mut data = Vec::with_capacity(4 + body.len());
        data.extend_from_slice(&transaction.to_le_bytes());
        data.extend_from_slice(&status.to_le_bytes());
        data.extend_from_slice(body);
        client.authenticate_sae(ifindex, &bss.ssid, bss.bssid, bss.frequency, &data).map_err(step("AUTHENTICATE"))
    };
    send(client, 1, method.commit_status(), &exchange.commit_body(None))?;
    log::info!("sae: commit sent to {} ({method:?})", bss.bssid);
    let (mut sent_confirm, mut token_sent) = (false, false);
    loop {
        let Some(event) = client.next_event(deadline, clock).map_err(step("waiting for SAE frames"))? else {
            return Err(ConnectError::Expired { step: "SAE exchange", limit });
        };
        if event.ifindex() != Some(ifindex) {
            continue;
        }
        let (frame, timed_out) = match event {
            Event::Authenticate { frame, timed_out, .. } => (frame, timed_out),
            Event::Disconnect { reason, by_ap, .. } => {
                return Err(ConnectError::Disconnected { step: "SAE", reason, by_ap });
            },
            other => {
                log::debug!("sae: {other:?}");
                continue;
            },
        };
        if timed_out {
            return Err(ConnectError::Expired { step: "SAE answer", limit });
        }
        let Some(auth) = frame.get(AUTH_HEADER_LEN..).and_then(AuthFrame::parse) else { continue };
        if auth.algorithm != sae::AUTH_ALGORITHM {
            continue;
        }
        let rejected = |status: u16| ConnectError::SaeRejected { status: StatusCode::from_raw(status) };
        match (auth.transaction, auth.status) {
            (1, sae::STATUS_ANTI_CLOGGING_TOKEN_REQUIRED) if !token_sent => {
                let Some(token) = sae::anti_clogging_token(auth.body) else { return Err(rejected(auth.status)) };
                token_sent = true;
                send(client, 1, method.commit_status(), &exchange.commit_body(Some(token)))?;
                log::info!("sae: the access point asked for an anti-clogging token; commit sent again");
            },
            (1, status) if status == method.commit_status() => match exchange.process_commit(auth.body) {
                Ok(()) => {
                    let confirm = exchange.confirm_body().map_err(|source| ConnectError::Sae { source })?;
                    send(client, 2, sae::STATUS_SUCCESS, &confirm)?;
                    sent_confirm = true;
                    log::info!("sae: commit of {} accepted, confirm sent", bss.bssid);
                },
                Err(SaeError::Reflection) => log::warn!("sae: dropped a reflected commit"),
                Err(source) => return Err(ConnectError::Sae { source }),
            },
            (2, sae::STATUS_SUCCESS) if sent_confirm => {
                exchange.check_confirm(auth.body).map_err(|source| ConnectError::Sae { source })?;
                log::info!("sae: confirm of {} verified; the PMK is derived", bss.bssid);
                let (Some(pmk), Some(pmkid)) = (exchange.pmk().cloned(), exchange.pmkid()) else {
                    return Err(ConnectError::Sae { source: SaeError::NoPeer });
                };
                return Ok((pmk, pmkid));
            },
            (_, status) if status != sae::STATUS_SUCCESS && status != sae::STATUS_H2E => return Err(rejected(status)),
            (transaction, status) => {
                log::debug!("sae: ignored transaction {transaction} with status {status}");
            },
        }
    }
}

fn done(
    ifindex: IfIndex,
    ap: MacAddr,
    start: Instant,
    associated: Instant,
    clock: &dyn Fn() -> Instant,
    supplicant: Option<Supplicant>,
) -> Connection {
    Connection { ifindex, ap, associated_after: associated - start, handshake_after: clock() - associated, supplicant }
}

/// Feed one EAPOL frame to the supplicant, send its reply and install what it hands back; true once
/// the 4-way handshake completed and the port is open.
fn eapol(
    client: &mut Client<KernelSocket>,
    supplicant: &mut Supplicant,
    ifindex: IfIndex,
    ap: MacAddr,
    frame: &[u8],
) -> Result<bool, ConnectError> {
    let step_out = match supplicant.handle(frame, &mut KernelRng) {
        Ok(step_out) => step_out,
        Err(error) => {
            log_drop(&error);
            return Ok(false);
        },
    };
    log::info!("4-way: {:?} accepted, replying", step_out.message);
    client.send_eapol(ifindex, ap, &step_out.reply).map_err(step("sending EAPOL"))?;
    install(client, ifindex, ap, &step_out.install)?;
    if step_out.complete {
        client.authorize(ifindex, ap).map_err(step("SET_STATION authorized"))?;
        log::info!("4-way: complete; the port to {ap} is open");
    }
    Ok(step_out.complete)
}

fn log_drop(error: &HandshakeError) {
    match error {
        HandshakeError::Mic => log::error!("4-way: dropped a frame: {error}"),
        _ => log::warn!("4-way: dropped a frame: {error}"),
    }
}

fn install(
    client: &mut Client<KernelSocket>,
    ifindex: IfIndex,
    ap: MacAddr,
    keys: &[Install],
) -> Result<(), ConnectError> {
    for key in keys {
        match key {
            Install::Pairwise { tk } => {
                client
                    .new_key(ifindex, &KeyParams { peer: Some(ap), index: 0, cipher: CIPHER_CCMP, data: tk, seq: None })
                    .map_err(step("NEW_KEY pairwise"))?;
                log::info!("keys: pairwise CCMP key installed for {ap}");
            },
            Install::Group { gtk, rsc } => {
                client
                    .new_key(
                        ifindex,
                        &KeyParams {
                            peer: None,
                            index: gtk.index,
                            cipher: CIPHER_CCMP,
                            data: &gtk.key,
                            seq: Some(&rsc[..6]),
                        },
                    )
                    .map_err(step("NEW_KEY group"))?;
                client.set_default_multicast_key(ifindex, gtk.index).map_err(step("SET_KEY default multicast"))?;
                log::info!("keys: group CCMP key {} installed", gtk.index);
            },
            Install::Igtk(igtk) => {
                let index = igtk.index as u8;
                client
                    .new_key(
                        ifindex,
                        &KeyParams {
                            peer: None,
                            index,
                            cipher: CIPHER_AES_CMAC,
                            data: &igtk.key,
                            seq: Some(&igtk.ipn),
                        },
                    )
                    .map_err(step("NEW_KEY integrity group"))?;
                client.set_default_mgmt_key(ifindex, index).map_err(step("SET_KEY default management"))?;
                log::info!("keys: integrity group key {index} installed");
            },
        }
    }
    Ok(())
}

impl Connection {
    /// Keep the connection for at most `hold`, answering group rekeys, until `done` says so; checked at
    /// least every `HOLD_SLICE`.
    pub fn hold(
        &mut self,
        client: &mut Client<KernelSocket>,
        hold: Duration,
        done: &dyn Fn() -> bool,
        clock: &dyn Fn() -> Instant,
    ) -> Result<(), ConnectError> {
        self.serve(client, Some(clock() + hold), done, clock)
    }

    /// Keep the connection until `done` says so, answering group rekeys; an error when the link drops.
    pub fn watch(
        &mut self,
        client: &mut Client<KernelSocket>,
        done: &dyn Fn() -> bool,
        clock: &dyn Fn() -> Instant,
    ) -> Result<(), ConnectError> {
        self.serve(client, None, done, clock)
    }

    fn serve(
        &mut self,
        client: &mut Client<KernelSocket>,
        until: Option<Instant>,
        done: &dyn Fn() -> bool,
        clock: &dyn Fn() -> Instant,
    ) -> Result<(), ConnectError> {
        while !done() && until.is_none_or(|until| clock() < until) {
            let slice = clock() + HOLD_SLICE;
            let slice = until.map_or(slice, |until| slice.min(until));
            while let Some(event) = client.next_event(slice, clock).map_err(step("holding the connection"))? {
                match (event, self.supplicant.as_mut()) {
                    (Event::ControlPortFrame { ifindex, ethertype: ETH_P_PAE, frame, .. }, Some(supplicant))
                        if ifindex == self.ifindex =>
                    {
                        eapol(client, supplicant, self.ifindex, self.ap, &frame)?;
                    },
                    (Event::Disconnect { ifindex, reason, by_ap }, _) if ifindex == self.ifindex => {
                        return Err(ConnectError::Disconnected { step: "the hold", reason, by_ap });
                    },
                    _ => {},
                }
            }
        }
        Ok(())
    }

    /// Leave the connection; returns the reason the disconnect event gave.
    pub fn leave(
        self,
        client: &mut Client<KernelSocket>,
        limit: Duration,
        clock: &dyn Fn() -> Instant,
    ) -> Result<Option<ReasonCode>, ConnectError> {
        client.disconnect(self.ifindex, ReasonCode::DeauthLeaving).map_err(step("DISCONNECT"))?;
        let deadline = clock() + limit;
        loop {
            match client.next_event(deadline, clock).map_err(step("waiting for the disconnect event"))? {
                Some(Event::Disconnect { ifindex, reason, .. }) if ifindex == self.ifindex => return Ok(reason),
                Some(_) => {},
                None => return Err(ConnectError::Expired { step: "disconnect event", limit }),
            }
        }
    }
}
