//! The station side of the 4-way and group key handshakes.
//!
//! Frames go in as received (802.1X header on), replies come out the same way, and keys to install are
//! handed back rather than installed, so the caller owns every side effect. A frame that fails a check
//! is dropped with an error and changes no state, as 802.11 requires, so a forged or stale frame
//! cannot abort a handshake. The MIC is compared in constant time, the replay counter must grow over
//! every frame whose MIC verified, and a key already installed is never handed out again.

use alloc::vec::Vec;

use snafu::Snafu;

use crate::eapol::{FrameError, KeyFrame, KeyInfo, unwrap_key_data};
use crate::kdf::{KEY_LEN, NONCE_LEN, Pmk, Ptk};
use crate::rsn::{Akm, Gtk, Igtk, parse_key_data};
use crate::{MacAddr, Rng};

#[derive(Debug, Clone, PartialEq, Eq, Snafu)]
pub enum HandshakeError {
    #[snafu(display("malformed EAPOL-Key frame: {source}"))]
    Frame { source: FrameError },
    #[snafu(display("replay counter {got} is not above {last}, the last verified one"))]
    Replay { got: u64, last: u64 },
    #[snafu(display("the MIC does not verify (wrong passphrase, or a forged frame)"))]
    Mic,
    #[snafu(display("message 3's ANonce differs from message 1's"))]
    ANonce,
    #[snafu(display("message 3's RSN element differs from the beacon's"))]
    RsnMismatch,
    #[snafu(display("message 3's RSNX element differs from the beacon's"))]
    RsnxMismatch,
    #[snafu(display("the key data carries no GTK"))]
    NoGtk,
    #[snafu(display("the key data carries no IGTK, which protected management frames need"))]
    NoIgtk,
    #[snafu(display("a key frame with key information {info:#06x} is not expected now"))]
    Unexpected { info: u16 },
}

impl From<FrameError> for HandshakeError {
    fn from(source: FrameError) -> Self {
        Self::Frame { source }
    }
}

/// A key the caller installs in the driver.
#[derive(Clone, PartialEq, Eq)]
pub enum Install {
    /// The CCMP temporal key, for the AP's address, index 0.
    Pairwise { tk: [u8; KEY_LEN] },
    /// A group key and its receive sequence counter.
    Group { gtk: Gtk, rsc: [u8; 8] },
    /// The key that protects group-addressed management frames (BIP-CMAC-128).
    Igtk(Igtk),
}

impl core::fmt::Debug for Install {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Pairwise { .. } => f.write_str("Pairwise"),
            Self::Group { gtk, .. } => write!(f, "Group({gtk:?})"),
            Self::Igtk(igtk) => write!(f, "Igtk({igtk:?})"),
        }
    }
}

/// What a supplicant is told about the network and the connection.
pub struct Params<'a> {
    pub akm: Akm,
    pub pmk: Pmk,
    pub aa: MacAddr,
    pub spa: MacAddr,
    /// The RSN and RSNX elements of the AP's beacon, whole, which message 3 must repeat.
    pub ap_rsn: &'a [u8],
    pub ap_rsnx: Option<&'a [u8]>,
    /// The RSN and RSNX elements this station sent in its association request.
    pub own_rsn: &'a [u8],
    pub own_rsnx: Option<&'a [u8]>,
}

/// Which message a frame was taken as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Message {
    Pairwise1,
    Pairwise3,
    Group1,
}

/// What one accepted frame produced.
#[derive(Debug)]
pub struct Step {
    pub message: Message,
    /// The reply to send, 802.1X header on.
    pub reply: Vec<u8>,
    /// Keys to install, in order, before the port is opened.
    pub install: Vec<Install>,
    /// The 4-way handshake completed with this frame.
    pub complete: bool,
}

pub struct Supplicant {
    akm: Akm,
    pmk: Pmk,
    aa: MacAddr,
    spa: MacAddr,
    /// The elements of the AP's beacon, which message 3 must repeat.
    ap_rsn: Vec<u8>,
    ap_rsnx: Option<Vec<u8>>,
    /// The elements this station sent in its association request, which message 2 repeats.
    own_elements: Vec<u8>,
    /// The authenticator nonce and the PTK derived with it, between messages 1 and 3.
    pending: Option<([u8; NONCE_LEN], Ptk)>,
    ptk: Option<Ptk>,
    replay: Option<u64>,
    gtk: Option<Gtk>,
    igtk: Option<Igtk>,
}

impl Supplicant {
    pub fn new(params: Params<'_>) -> Self {
        let mut own_elements = params.own_rsn.to_vec();
        own_elements.extend_from_slice(params.own_rsnx.unwrap_or_default());
        Self {
            akm: params.akm,
            pmk: params.pmk,
            aa: params.aa,
            spa: params.spa,
            ap_rsn: params.ap_rsn.to_vec(),
            ap_rsnx: params.ap_rsnx.map(<[u8]>::to_vec),
            own_elements,
            pending: None,
            ptk: None,
            replay: None,
            gtk: None,
            igtk: None,
        }
    }

    /// Whether the 4-way handshake has completed.
    pub const fn is_complete(&self) -> bool {
        self.ptk.is_some()
    }

    /// `info` at the descriptor version the key management defines.
    fn info(&self, info: KeyInfo) -> KeyInfo {
        KeyInfo { version: self.akm.descriptor_version(), ..info }
    }

    fn check_replay(&self, got: u64) -> Result<(), HandshakeError> {
        match self.replay {
            Some(last) if got <= last => Err(HandshakeError::Replay { got, last }),
            _ => Ok(()),
        }
    }

    /// Take one received EAPOL-Key frame.
    pub fn handle(&mut self, raw: &[u8], rng: &mut dyn Rng) -> Result<Step, HandshakeError> {
        let frame = KeyFrame::parse(raw)?;
        let info = frame.info;
        if info.version != self.akm.descriptor_version() {
            return Err(FrameError::Version { version: info.version }.into());
        }
        match (info.pairwise, info.ack, info.mic) {
            (true, true, false) => self.message_1(&frame, rng),
            (true, true, true) if info.install && info.encrypted => self.message_3(raw, &frame),
            (false, true, true) if info.encrypted && self.ptk.is_some() => self.group_1(raw, &frame),
            _ => Err(HandshakeError::Unexpected { info: info.raw() }),
        }
    }

    fn message_1(&mut self, frame: &KeyFrame, rng: &mut dyn Rng) -> Result<Step, HandshakeError> {
        self.check_replay(frame.replay_counter)?;
        let mut snonce = [0; NONCE_LEN];
        rng.fill(&mut snonce);
        let tptk = Ptk::derive(self.akm, &self.pmk, self.aa, self.spa, &frame.nonce, &snonce);
        let mut reply =
            KeyFrame::new(self.info(KeyInfo { pairwise: true, mic: true, ..KeyInfo::NONE }), frame.replay_counter);
        reply.nonce = snonce;
        reply.key_data.clone_from(&self.own_elements);
        let reply = reply.sign(&tptk.kck);
        self.pending = Some((frame.nonce, tptk));
        Ok(Step { message: Message::Pairwise1, reply, install: Vec::new(), complete: false })
    }

    fn message_3(&mut self, raw: &[u8], frame: &KeyFrame) -> Result<Step, HandshakeError> {
        self.check_replay(frame.replay_counter)?;
        let Some((anonce, tptk)) = &self.pending else {
            return Err(HandshakeError::Unexpected { info: frame.info.raw() });
        };
        if *anonce != frame.nonce {
            return Err(HandshakeError::ANonce);
        }
        if !KeyFrame::verify(raw, &tptk.kck) {
            return Err(HandshakeError::Mic);
        }
        let plain = unwrap_key_data(&tptk.kek, &frame.key_data)?;
        let data = parse_key_data(&plain);
        if data.rsn != Some(self.ap_rsn.as_slice()) {
            return Err(HandshakeError::RsnMismatch);
        }
        if data.rsnx != self.ap_rsnx.as_deref() {
            return Err(HandshakeError::RsnxMismatch);
        }
        let gtk = data.gtk.ok_or(HandshakeError::NoGtk)?;
        let igtk = if self.akm.requires_pmf() { Some(data.igtk.ok_or(HandshakeError::NoIgtk)?) } else { None };
        let mut reply = KeyFrame::new(
            self.info(KeyInfo { pairwise: true, mic: true, secure: true, ..KeyInfo::NONE }),
            frame.replay_counter,
        );
        let reply = reply.sign(&tptk.kck);
        let mut install = Vec::new();
        if self.ptk.as_ref() != Some(tptk) {
            install.push(Install::Pairwise { tk: tptk.tk });
        }
        if self.gtk.as_ref() != Some(&gtk) {
            install.push(Install::Group { gtk: gtk.clone(), rsc: frame.rsc });
        }
        if let Some(igtk) = &igtk
            && self.igtk.as_ref() != Some(igtk)
        {
            install.push(Install::Igtk(igtk.clone()));
        }
        self.replay = Some(frame.replay_counter);
        self.ptk = Some(tptk.clone());
        self.gtk = Some(gtk);
        self.igtk = igtk;
        Ok(Step { message: Message::Pairwise3, reply, install, complete: true })
    }

    fn group_1(&mut self, raw: &[u8], frame: &KeyFrame) -> Result<Step, HandshakeError> {
        self.check_replay(frame.replay_counter)?;
        let Some(ptk) = &self.ptk else { return Err(HandshakeError::Unexpected { info: frame.info.raw() }) };
        if !KeyFrame::verify(raw, &ptk.kck) {
            return Err(HandshakeError::Mic);
        }
        let plain = unwrap_key_data(&ptk.kek, &frame.key_data)?;
        let data = parse_key_data(&plain);
        if data.gtk.is_none() && data.igtk.is_none() {
            return Err(HandshakeError::NoGtk);
        }
        let reply =
            KeyFrame::new(self.info(KeyInfo { mic: true, secure: true, ..KeyInfo::NONE }), frame.replay_counter)
                .sign(&ptk.kck);
        let mut install = Vec::new();
        if let Some(gtk) = data.gtk
            && self.gtk.as_ref() != Some(&gtk)
        {
            install.push(Install::Group { gtk: gtk.clone(), rsc: frame.rsc });
            self.gtk = Some(gtk);
        }
        if let Some(igtk) = data.igtk
            && self.igtk.as_ref() != Some(&igtk)
        {
            install.push(Install::Igtk(igtk.clone()));
            self.igtk = Some(igtk);
        }
        self.replay = Some(frame.replay_counter);
        Ok(Step { message: Message::Group1, reply, install, complete: false })
    }
}
