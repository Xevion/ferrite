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
use crate::rsn::{Gtk, parse_key_data};
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
    #[snafu(display("the key data carries no GTK"))]
    NoGtk,
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
}

impl core::fmt::Debug for Install {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Pairwise { .. } => f.write_str("Pairwise"),
            Self::Group { gtk, .. } => write!(f, "Group({gtk:?})"),
        }
    }
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
    pmk: Pmk,
    aa: MacAddr,
    spa: MacAddr,
    /// The RSN element of the AP's beacon, which message 3 must repeat.
    ap_rsn: Vec<u8>,
    /// The RSN element this station sent in its association request.
    own_rsn: Vec<u8>,
    /// The authenticator nonce and the PTK derived with it, between messages 1 and 3.
    pending: Option<([u8; NONCE_LEN], Ptk)>,
    ptk: Option<Ptk>,
    replay: Option<u64>,
    gtk: Option<Gtk>,
}

impl Supplicant {
    pub fn new(pmk: Pmk, aa: MacAddr, spa: MacAddr, ap_rsn: &[u8], own_rsn: &[u8]) -> Self {
        Self {
            pmk,
            aa,
            spa,
            ap_rsn: ap_rsn.to_vec(),
            own_rsn: own_rsn.to_vec(),
            pending: None,
            ptk: None,
            replay: None,
            gtk: None,
        }
    }

    /// Whether the 4-way handshake has completed.
    pub const fn is_complete(&self) -> bool {
        self.ptk.is_some()
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
        if info.version != crate::eapol::DescriptorVersion::Sha1Aes {
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
        let tptk = Ptk::derive(&self.pmk, self.aa, self.spa, &frame.nonce, &snonce);
        let mut reply = KeyFrame::new(KeyInfo { pairwise: true, mic: true, ..KeyInfo::NONE }, frame.replay_counter);
        reply.nonce = snonce;
        reply.key_data.clone_from(&self.own_rsn);
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
        let gtk = data.gtk.ok_or(HandshakeError::NoGtk)?;
        let mut reply =
            KeyFrame::new(KeyInfo { pairwise: true, mic: true, secure: true, ..KeyInfo::NONE }, frame.replay_counter);
        let reply = reply.sign(&tptk.kck);
        let mut install = Vec::new();
        if self.ptk.as_ref() != Some(tptk) {
            install.push(Install::Pairwise { tk: tptk.tk });
        }
        if self.gtk.as_ref() != Some(&gtk) {
            install.push(Install::Group { gtk: gtk.clone(), rsc: frame.rsc });
        }
        self.replay = Some(frame.replay_counter);
        self.ptk = Some(tptk.clone());
        self.gtk = Some(gtk);
        Ok(Step { message: Message::Pairwise3, reply, install, complete: true })
    }

    fn group_1(&mut self, raw: &[u8], frame: &KeyFrame) -> Result<Step, HandshakeError> {
        self.check_replay(frame.replay_counter)?;
        let Some(ptk) = &self.ptk else { return Err(HandshakeError::Unexpected { info: frame.info.raw() }) };
        if !KeyFrame::verify(raw, &ptk.kck) {
            return Err(HandshakeError::Mic);
        }
        let plain = unwrap_key_data(&ptk.kek, &frame.key_data)?;
        let gtk = parse_key_data(&plain).gtk.ok_or(HandshakeError::NoGtk)?;
        let reply =
            KeyFrame::new(KeyInfo { mic: true, secure: true, ..KeyInfo::NONE }, frame.replay_counter).sign(&ptk.kck);
        let mut install = Vec::new();
        if self.gtk.as_ref() != Some(&gtk) {
            install.push(Install::Group { gtk: gtk.clone(), rsc: frame.rsc });
        }
        self.replay = Some(frame.replay_counter);
        self.gtk = Some(gtk);
        Ok(Step { message: Message::Group1, reply, install, complete: false })
    }
}
