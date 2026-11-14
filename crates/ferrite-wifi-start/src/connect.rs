//! A WPA2-PSK connection as iwd makes one: `NL80211_CMD_CONNECT` hands authentication and association
//! to cfg80211's SME, EAPOL comes back over nl80211 to the owning socket, the 4-way handshake runs in
//! Rust (`ferrite-wpa`), and the keys and the open port go back through nl80211.

use alloc::vec::Vec;
use core::ffi::c_void;
use core::time::Duration;

use ferrite_hal::Instant;
use ferrite_nl80211::client::{CIPHER_CCMP, Client, ClientError, ConnectParams, KeyParams};
use ferrite_nl80211::mlme::{Event, ReasonCode, StatusCode};
use ferrite_nl80211::nl80211::{Bss, IfIndex, MacAddr};
use ferrite_wpa::eapol::ETH_P_PAE;
use ferrite_wpa::kdf::Pmk;
use ferrite_wpa::rsn::Rsn;
use ferrite_wpa::supplicant::{HandshakeError, Install, Supplicant};
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
    #[snafu(display("disconnected during {step}: reason {}{}", reason.map_or_else(|| alloc::string::String::from("none"), |r| alloc::format!("{r}")), if *by_ap { ", by the AP" } else { "" }))]
    Disconnected { step: &'static str, reason: Option<ReasonCode>, by_ap: bool },
    #[snafu(display("no {step} within {} ms", limit.as_millis()))]
    Expired { step: &'static str, limit: Duration },
    #[snafu(display("the BSS advertises no RSN element"))]
    NoRsn,
    #[snafu(display("the BSS does not offer WPA2-PSK with CCMP"))]
    Unsupported,
}

const HOLD_SLICE: Duration = Duration::from_millis(100);

fn step(step: &'static str) -> impl FnOnce(ClientError) -> ConnectError {
    move |source| ConnectError::Nl80211 { step, source }
}

/// A completed connection.
pub struct Connection {
    pub ifindex: IfIndex,
    pub ap: MacAddr,
    /// From the connect request to the connect event.
    pub associated_after: Duration,
    /// From the connect event to the port opening.
    pub handshake_after: Duration,
    supplicant: Supplicant,
}

/// Connect `ifindex` (with address `own`) to `bss` and run the 4-way handshake, within `limit`.
pub fn connect(
    client: &mut Client<KernelSocket>,
    ifindex: IfIndex,
    own: MacAddr,
    bss: &Bss,
    pmk: Pmk,
    limit: Duration,
    clock: &dyn Fn() -> Instant,
) -> Result<Connection, ConnectError> {
    let ap_rsn = bss.rsn.as_deref().ok_or(ConnectError::NoRsn)?;
    let offered = Rsn::parse(&ap_rsn[2..]).map_err(|_| ConnectError::Unsupported)?;
    if !offered.supports_psk_ccmp() {
        return Err(ConnectError::Unsupported);
    }
    let own_rsn = Rsn::psk_ccmp().element();
    client.join_mlme().map_err(step("joining the mlme group"))?;
    let start = clock();
    let deadline = start + limit;
    client
        .connect_network(
            ifindex,
            &ConnectParams { ssid: &bss.ssid, bssid: Some(bss.bssid), frequency: Some(bss.frequency), rsn: &own_rsn },
        )
        .map_err(step("CONNECT"))?;
    log::info!("connect: CONNECT accepted for {} on {} MHz", bss.bssid, bss.frequency.0);

    let mut supplicant = Supplicant::new(pmk, wpa_mac(bss.bssid), wpa_mac(own), ap_rsn, &own_rsn);
    let mut early: Vec<Vec<u8>> = Vec::new();
    let mut associated_at = None;
    loop {
        let what = if associated_at.is_some() { "4-way handshake" } else { "connect event" };
        let Some(event) = client.next_event(deadline, clock).map_err(step("waiting for events"))? else {
            return Err(ConnectError::Expired { step: what, limit });
        };
        if event.ifindex() != Some(ifindex) {
            continue;
        }
        match event {
            Event::Connect { status: StatusCode::Success, .. } => {
                let at = clock();
                associated_at = Some(at);
                log::info!("connect: associated after {} ms (status 0)", (at - start).as_millis());
                for frame in core::mem::take(&mut early) {
                    if eapol(client, &mut supplicant, ifindex, bss.bssid, &frame)? {
                        return Ok(done(ifindex, bss.bssid, start, at, clock, supplicant));
                    }
                }
            },
            Event::Connect { status, timed_out, .. } => return Err(ConnectError::Rejected { status, timed_out }),
            Event::Disconnect { reason, by_ap, .. } => {
                return Err(ConnectError::Disconnected { step: what, reason, by_ap });
            },
            Event::ControlPortFrame { ethertype: ETH_P_PAE, frame, .. } => match associated_at {
                None => early.push(frame),
                Some(at) => {
                    if eapol(client, &mut supplicant, ifindex, bss.bssid, &frame)? {
                        return Ok(done(ifindex, bss.bssid, start, at, clock, supplicant));
                    }
                },
            },
            other => log::debug!("connect: {other:?}"),
        }
    }
}

fn done(
    ifindex: IfIndex,
    ap: MacAddr,
    start: Instant,
    associated: Instant,
    clock: &dyn Fn() -> Instant,
    supplicant: Supplicant,
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
                match event {
                    Event::ControlPortFrame { ifindex, ethertype: ETH_P_PAE, frame, .. } if ifindex == self.ifindex => {
                        eapol(client, &mut self.supplicant, self.ifindex, self.ap, &frame)?;
                    },
                    Event::Disconnect { ifindex, reason, by_ap } if ifindex == self.ifindex => {
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
