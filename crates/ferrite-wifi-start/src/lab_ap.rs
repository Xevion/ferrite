//! A minimal WPA2-PSK access point for the lab, in the place of hostapd: mac80211 in AP mode leaves
//! authentication, association and the 4-way handshake to userspace. On the AP's own nl80211 socket it
//! starts beaconing with an RSN element, installs the group key, registers for authentication and
//! (re)association requests, answers them with `NL80211_CMD_FRAME`, admits the station with
//! `NEW_STATION`, runs the authenticator side of the 4-way handshake over the control port, then
//! installs the pairwise key and authorizes the station. It runs on a kthread of its own, as hostapd is
//! a process of its own, so the station's blocking client and the AP make progress side by side.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ffi::{c_int, c_void};
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;

use ferrite_lkpi::sched::SchedHandle;
use ferrite_nl80211::client::{ApConfig, CIPHER_CCMP, Client, KeyParams, StationParams};
use ferrite_nl80211::mlme::Event;
use ferrite_nl80211::nl80211::{Frequency, IfIndex, MacAddr, elements};
use ferrite_wpa::Rng as _;
use ferrite_wpa::authenticator::{AuthStep, Authenticator};
use ferrite_wpa::eapol::ETH_P_PAE;
use ferrite_wpa::kdf::Pmk;
use ferrite_wpa::rsn::{ELEMENT_RSN, Gtk, Rsn};

use crate::connect::{KernelRng, wpa_mac};
use crate::scan::{KernelSocket, ScanError};

/// Frame control values (type and subtype) of the management frames the AP handles.
const FC_ASSOC_REQ: u16 = 0x0000;
const FC_ASSOC_RESP: u16 = 0x0010;
const FC_REASSOC_REQ: u16 = 0x0020;
const FC_REASSOC_RESP: u16 = 0x0030;
const FC_DISASSOC: u16 = 0x00a0;
const FC_AUTH: u16 = 0x00b0;
const FC_DEAUTH: u16 = 0x00c0;
const HEADER_LEN: usize = 24;
const ELEMENT_RATES: u8 = 1;
const ELEMENT_EXT_RATES: u8 = 50;
const RATES: [u8; 4] = [0x82, 0x84, 0x8b, 0x96];
const GTK_INDEX: u8 = 1;
const POLL: Duration = Duration::from_millis(100);

/// Set by the AP thread as the handshake lands, for the init task's checks.
pub static PORT_OPEN: AtomicBool = AtomicBool::new(false);
pub static GROUP_KEY_INSTALLED: AtomicBool = AtomicBool::new(false);

struct Ap {
    client: Client<KernelSocket>,
    sched: SchedHandle,
    ifindex: IfIndex,
    mac: MacAddr,
    pmk: Pmk,
    rsn: Vec<u8>,
    gtk: Gtk,
    station: Option<Station>,
}

struct Station {
    mac: MacAddr,
    authenticator: Authenticator,
    /// The association response's cookie, whose acknowledged transmit status starts the handshake.
    assoc_cookie: Option<u64>,
}

/// Start beaconing `ssid` as WPA2-PSK on 2412 MHz from `ifindex`, install the group key and register
/// for the frames hostapd handles. The mlme group carries the transmit status of sent frames.
pub fn start(
    mut client: Client<KernelSocket>,
    sched: SchedHandle,
    ifindex: IfIndex,
    mac: MacAddr,
    ssid: &[u8],
    pmk: Pmk,
) -> Result<ApTask, ScanError> {
    client.join_mlme()?;
    let rsn = Rsn::psk_ccmp().element();
    let ap = ApConfig { ssid, rsn: Some(&rsn), frequency: Frequency(2412), beacon_interval: 10, channel: 1 };
    client.start_ap(ifindex, mac, &ap)?;
    let mut key = alloc::vec![0; 16];
    KernelRng.fill(&mut key);
    let gtk = Gtk { index: GTK_INDEX, key };
    client.new_key(
        ifindex,
        &KeyParams { peer: None, index: GTK_INDEX, cipher: CIPHER_CCMP, data: &gtk.key, seq: None },
    )?;
    client.set_default_multicast_key(ifindex, GTK_INDEX)?;
    GROUP_KEY_INSTALLED.store(true, Ordering::Release);
    for frame_type in [FC_AUTH, FC_ASSOC_REQ, FC_REASSOC_REQ, FC_DEAUTH, FC_DISASSOC] {
        client.register_frame(ifindex, frame_type)?;
    }
    log::info!(
        "lab AP: {mac} beacons the WPA2-PSK network \"{}\" on 2412 MHz; group key {GTK_INDEX} installed",
        ferrite_nl80211::nl80211::SsidDisplay(ssid)
    );
    let state = Box::new(Ap { client, sched, ifindex, mac, pmk, rsn, gtk, station: None });
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
        let sched = ap.sched.clone();
        let clock = || sched.now();
        match ap.client.next_event(clock() + POLL, &clock) {
            Ok(Some(event)) if event.ifindex() == Some(ap.ifindex) => {
                if let Err(error) = ap.event(event) {
                    log::error!("lab AP: {error}");
                }
            },
            Ok(_) => {},
            Err(error) => log::error!("lab AP: {error}"),
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

impl Ap {
    fn event(&mut self, event: Event) -> Result<(), ferrite_nl80211::client::ClientError> {
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
                    _ => {
                        log::info!("lab AP: {source} left (frame control {fc:#06x})");
                        self.station = None;
                        Ok(())
                    },
                }
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
                let message_1 = station.authenticator.start(&mut KernelRng);
                let dest = station.mac;
                self.client.send_eapol(self.ifindex, dest, &message_1)?;
                log::info!("lab AP: 4-way message 1 sent to {dest}");
                Ok(())
            },
            Event::ControlPortFrame { source, ethertype: ETH_P_PAE, frame, .. } => self.eapol(source, &frame),
            _ => Ok(()),
        }
    }

    fn auth(&mut self, source: MacAddr, body: &[u8]) -> Result<(), ferrite_nl80211::client::ClientError> {
        let (alg, seq) = match body {
            [a0, a1, s0, s1, ..] => (u16::from_le_bytes([*a0, *a1]), u16::from_le_bytes([*s0, *s1])),
            _ => return Ok(()),
        };
        if alg != 0 || seq != 1 {
            return Ok(());
        }
        let mut frame = header(FC_AUTH, source, self.mac);
        frame.extend_from_slice(&[0, 0, 2, 0, 0, 0]);
        self.client.send_frame(self.ifindex, &frame)?;
        log::info!("lab AP: open system authentication of {source} answered with status 0");
        Ok(())
    }

    fn assoc(
        &mut self,
        source: MacAddr,
        ies: &[u8],
        body: &[u8],
        reassoc: bool,
    ) -> Result<(), ferrite_nl80211::client::ClientError> {
        let (capability, listen_interval) = match body {
            [c0, c1, l0, l1, ..] => (u16::from_le_bytes([*c0, *c1]), u16::from_le_bytes([*l0, *l1])),
            _ => return Ok(()),
        };
        let mut rates = Vec::new();
        let mut sta_rsn = None;
        for (id, data) in elements(ies) {
            match id {
                ELEMENT_RATES | ELEMENT_EXT_RATES => rates.extend_from_slice(data),
                ELEMENT_RSN => {
                    let mut element = alloc::vec![ELEMENT_RSN, data.len() as u8];
                    element.extend_from_slice(data);
                    sta_rsn = Some(element);
                },
                _ => {},
            }
        }
        let Some(sta_rsn) = sta_rsn else {
            log::warn!("lab AP: {source} associated without an RSN element; ignored");
            return Ok(());
        };
        self.client.new_station(
            self.ifindex,
            &StationParams { mac: source, aid: 1, listen_interval, capability, supported_rates: &rates },
        )?;
        let mut frame = header(if reassoc { FC_REASSOC_RESP } else { FC_ASSOC_RESP }, source, self.mac);
        frame.extend_from_slice(&0x0011_u16.to_le_bytes());
        frame.extend_from_slice(&0_u16.to_le_bytes());
        frame.extend_from_slice(&(1_u16 | 0xc000).to_le_bytes());
        frame.extend_from_slice(&[ELEMENT_RATES, RATES.len() as u8]);
        frame.extend_from_slice(&RATES);
        let cookie = self.client.send_frame(self.ifindex, &frame)?;
        let authenticator = Authenticator::new(
            self.pmk.clone(),
            wpa_mac(self.mac),
            wpa_mac(source),
            &self.rsn,
            &sta_rsn,
            self.gtk.clone(),
        );
        self.station = Some(Station { mac: source, authenticator, assoc_cookie: Some(cookie) });
        log::info!("lab AP: {source} associated with AID 1; station added");
        Ok(())
    }

    fn eapol(&mut self, source: MacAddr, frame: &[u8]) -> Result<(), ferrite_nl80211::client::ClientError> {
        let Some(station) = self.station.as_mut().filter(|s| s.mac == source) else { return Ok(()) };
        match station.authenticator.handle(frame) {
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
