//! The access point side of the 4-way and group key handshakes, as hostapd runs them: for the lab's
//! access point and for testing the supplicant against an independent state machine.

use alloc::vec::Vec;

use snafu::Snafu;

use crate::eapol::{FrameError, KeyFrame, KeyInfo, wrap_key_data};
use crate::kdf::{KEY_LEN, NONCE_LEN, Pmk, Ptk};
use crate::rsn::{Gtk, gtk_kde, parse_key_data};
use crate::{MacAddr, Rng};

#[derive(Debug, Clone, PartialEq, Eq, Snafu)]
pub enum AuthenticatorError {
    #[snafu(display("malformed EAPOL-Key frame: {source}"))]
    Frame { source: FrameError },
    #[snafu(display("replay counter {got}, expected {want}"))]
    Replay { got: u64, want: u64 },
    #[snafu(display("the MIC does not verify (the station has another passphrase)"))]
    Mic,
    #[snafu(display("message 2's RSN element differs from the association request's"))]
    RsnMismatch,
    #[snafu(display("a key frame with key information {info:#06x} is not expected now"))]
    Unexpected { info: u16 },
}

impl From<FrameError> for AuthenticatorError {
    fn from(source: FrameError) -> Self {
        Self::Frame { source }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Idle,
    SentMessage1,
    SentMessage3,
    Done,
    SentGroup1,
}

/// What one accepted frame produced.
#[derive(Debug, PartialEq, Eq)]
pub enum AuthStep {
    /// Message 2 verified: send message 3.
    Reply(Vec<u8>),
    /// Message 4 verified: install this pairwise key for the station and open its port.
    Complete { tk: [u8; KEY_LEN] },
    /// The group handshake's message 2 verified.
    GroupComplete,
}

pub struct Authenticator {
    pmk: Pmk,
    aa: MacAddr,
    spa: MacAddr,
    own_rsn: Vec<u8>,
    sta_rsn: Vec<u8>,
    gtk: Gtk,
    replay: u64,
    anonce: [u8; NONCE_LEN],
    ptk: Option<Ptk>,
    state: State,
}

impl Authenticator {
    /// `own_rsn` is the beacon's RSN element, `sta_rsn` the one in the station's association request.
    pub fn new(pmk: Pmk, aa: MacAddr, spa: MacAddr, own_rsn: &[u8], sta_rsn: &[u8], gtk: Gtk) -> Self {
        Self {
            pmk,
            aa,
            spa,
            own_rsn: own_rsn.to_vec(),
            sta_rsn: sta_rsn.to_vec(),
            gtk,
            replay: 0,
            anonce: [0; NONCE_LEN],
            ptk: None,
            state: State::Idle,
        }
    }

    pub const fn replay_counter(&self) -> u64 {
        self.replay
    }

    /// Message 1 with a fresh authenticator nonce.
    pub fn start(&mut self, rng: &mut dyn Rng) -> Vec<u8> {
        rng.fill(&mut self.anonce);
        self.replay += 1;
        let mut frame = KeyFrame::new(KeyInfo { pairwise: true, ack: true, ..KeyInfo::NONE }, self.replay);
        frame.key_length = KEY_LEN as u16;
        frame.nonce = self.anonce;
        self.state = State::SentMessage1;
        frame.to_bytes()
    }

    /// A group key handshake's message 1 carrying `gtk`.
    pub fn rekey(&mut self, gtk: Gtk) -> Result<Vec<u8>, AuthenticatorError> {
        let Some(ptk) = &self.ptk else { return Err(AuthenticatorError::Unexpected { info: 0 }) };
        self.replay += 1;
        let mut frame = KeyFrame::new(
            KeyInfo { ack: true, mic: true, secure: true, encrypted: true, ..KeyInfo::NONE },
            self.replay,
        );
        frame.key_data = wrap_key_data(&ptk.kek, &gtk_kde(&gtk));
        let bytes = frame.sign(&ptk.kck);
        self.gtk = gtk;
        self.state = State::SentGroup1;
        Ok(bytes)
    }

    /// Message 3 again, under a new replay counter, as an authenticator retransmits it.
    pub fn resend_message_3(&mut self) -> Result<Vec<u8>, AuthenticatorError> {
        let Some(ptk) = self.ptk.clone() else { return Err(AuthenticatorError::Unexpected { info: 0 }) };
        Ok(self.message_3(&ptk))
    }

    fn message_3(&mut self, ptk: &Ptk) -> Vec<u8> {
        self.replay += 1;
        let mut frame = KeyFrame::new(
            KeyInfo {
                pairwise: true,
                install: true,
                ack: true,
                mic: true,
                secure: true,
                encrypted: true,
                ..KeyInfo::NONE
            },
            self.replay,
        );
        frame.key_length = KEY_LEN as u16;
        frame.nonce = self.anonce;
        let mut plain = self.own_rsn.clone();
        plain.extend_from_slice(&gtk_kde(&self.gtk));
        frame.key_data = wrap_key_data(&ptk.kek, &plain);
        self.state = State::SentMessage3;
        frame.sign(&ptk.kck)
    }

    pub fn handle(&mut self, raw: &[u8]) -> Result<AuthStep, AuthenticatorError> {
        let frame = KeyFrame::parse(raw)?;
        if frame.replay_counter != self.replay {
            return Err(AuthenticatorError::Replay { got: frame.replay_counter, want: self.replay });
        }
        let info = frame.info;
        match (self.state, info.pairwise, info.mic, info.secure) {
            (State::SentMessage1, true, true, false) => {
                let ptk = Ptk::derive(&self.pmk, self.aa, self.spa, &self.anonce, &frame.nonce);
                if !KeyFrame::verify(raw, &ptk.kck) {
                    return Err(AuthenticatorError::Mic);
                }
                if parse_key_data(&frame.key_data).rsn != Some(self.sta_rsn.as_slice()) {
                    return Err(AuthenticatorError::RsnMismatch);
                }
                let reply = self.message_3(&ptk);
                self.ptk = Some(ptk);
                Ok(AuthStep::Reply(reply))
            },
            (State::SentMessage3, true, true, true) => {
                let ptk = self.ptk.as_ref().ok_or_else(|| AuthenticatorError::Unexpected { info: info.raw() })?;
                if !KeyFrame::verify(raw, &ptk.kck) {
                    return Err(AuthenticatorError::Mic);
                }
                self.state = State::Done;
                Ok(AuthStep::Complete { tk: ptk.tk })
            },
            (State::SentGroup1, false, true, true) => {
                let ptk = self.ptk.as_ref().ok_or_else(|| AuthenticatorError::Unexpected { info: info.raw() })?;
                if !KeyFrame::verify(raw, &ptk.kck) {
                    return Err(AuthenticatorError::Mic);
                }
                self.state = State::Done;
                Ok(AuthStep::GroupComplete)
            },
            _ => Err(AuthenticatorError::Unexpected { info: info.raw() }),
        }
    }
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};

    use super::*;
    use crate::kdf::{Passphrase, Ssid};
    use crate::rsn::Rsn;
    use crate::supplicant::{HandshakeError, Install, Message, Supplicant};

    /// Counter bytes, so each run's nonces differ and are reproducible.
    struct Counter(u8);

    impl Rng for Counter {
        fn fill(&mut self, buf: &mut [u8]) {
            for byte in buf {
                self.0 = self.0.wrapping_add(1);
                *byte = self.0;
            }
        }
    }

    const AA: MacAddr = MacAddr([2, 0, 0, 0, 1, 0]);
    const SPA: MacAddr = MacAddr([2, 0, 0, 0, 0, 0]);

    fn pmk(passphrase: &[u8]) -> Pmk {
        Pmk::from_passphrase(&Passphrase::new(passphrase).unwrap(), &Ssid::new(b"ferrite-lab").unwrap())
    }

    fn pair(sta_passphrase: &[u8]) -> (Authenticator, Supplicant) {
        let rsn = Rsn::psk_ccmp().element();
        let gtk = Gtk { index: 1, key: alloc::vec![0x5a; 16] };
        let ap = Authenticator::new(pmk(b"lab passphrase"), AA, SPA, &rsn, &rsn, gtk);
        (ap, Supplicant::new(pmk(sta_passphrase), AA, SPA, &rsn, &rsn))
    }

    #[test]
    fn the_four_way_handshake_agrees_on_the_keys_and_installs_each_once() {
        let (mut ap, mut sta) = pair(b"lab passphrase");
        let (mut ap_rng, mut sta_rng) = (Counter(0), Counter(100));
        let step = sta.handle(&ap.start(&mut ap_rng), &mut sta_rng).unwrap();
        check!(step.message == Message::Pairwise1);
        assert!(let Ok(AuthStep::Reply(message_3)) = ap.handle(&step.reply));
        let step = sta.handle(&message_3, &mut sta_rng).unwrap();
        check!(step.complete);
        assert!(let [Install::Pairwise { tk: sta_tk }, Install::Group { gtk, .. }] = step.install.as_slice());
        check!(gtk.index == 1);
        assert!(let Ok(AuthStep::Complete { tk: ap_tk }) = ap.handle(&step.reply));
        check!(*sta_tk == ap_tk);

        // A retransmitted message 3 (lost message 4) is answered but reinstalls nothing.
        let again = ap.resend_message_3().unwrap();
        let step = sta.handle(&again, &mut sta_rng).unwrap();
        check!(step.install.is_empty());
        assert!(let Ok(AuthStep::Complete { .. }) = ap.handle(&step.reply));

        let rekey = ap.rekey(Gtk { index: 2, key: alloc::vec![0xa5; 16] }).unwrap();
        let step = sta.handle(&rekey, &mut sta_rng).unwrap();
        check!(step.message == Message::Group1);
        assert!(let [Install::Group { gtk, .. }] = step.install.as_slice());
        check!((gtk.index, gtk.key[0]) == (2, 0xa5));
        check!(ap.handle(&step.reply) == Ok(AuthStep::GroupComplete));
    }

    #[test]
    fn a_wrong_passphrase_fails_at_the_authenticator_and_never_completes() {
        let (mut ap, mut sta) = pair(b"not the lab passphrase");
        let mut rng = Counter(0);
        let step = sta.handle(&ap.start(&mut rng), &mut rng).unwrap();
        check!(ap.handle(&step.reply) == Err(AuthenticatorError::Mic));
        check!(!sta.is_complete());
    }

    #[test]
    fn a_message_3_from_an_ap_with_another_passphrase_fails_the_mic_and_installs_nothing() {
        let (mut good_ap, _) = pair(b"lab passphrase");
        let (_, mut sta) = pair(b"other passphrase");
        let mut rng = Counter(0);
        let message_1 = good_ap.start(&mut rng);
        let step = sta.handle(&message_1, &mut rng).unwrap();
        // Forge message 3 with the AP's own view: the station derived a different PTK.
        let mut forger = Authenticator::new(
            pmk(b"lab passphrase"),
            AA,
            SPA,
            &Rsn::psk_ccmp().element(),
            &Rsn::psk_ccmp().element(),
            Gtk { index: 1, key: alloc::vec![1; 16] },
        );
        forger.anonce = good_ap.anonce;
        forger.replay = good_ap.replay;
        forger.state = State::SentMessage1;
        let snonce = KeyFrame::parse(&step.reply).unwrap().nonce;
        let ptk = Ptk::derive(&forger.pmk, AA, SPA, &forger.anonce, &snonce);
        let message_3 = forger.message_3(&ptk);
        check!(sta.handle(&message_3, &mut rng).unwrap_err() == HandshakeError::Mic);
        check!(!sta.is_complete());
    }

    #[test]
    fn a_replayed_message_3_is_dropped_without_a_reply() {
        let (mut ap, mut sta) = pair(b"lab passphrase");
        let mut rng = Counter(0);
        let step = sta.handle(&ap.start(&mut rng), &mut rng).unwrap();
        let AuthStep::Reply(message_3) = ap.handle(&step.reply).unwrap() else { panic!("no message 3") };
        sta.handle(&message_3, &mut rng).unwrap();
        let replay = ap.replay_counter();
        check!(sta.handle(&message_3, &mut rng).unwrap_err() == HandshakeError::Replay { got: replay, last: replay });
    }

    #[test]
    fn a_message_3_whose_rsn_element_differs_from_the_beacon_is_refused() {
        let rsn = Rsn::psk_ccmp().element();
        let mut other = Rsn::psk_ccmp();
        other.capabilities = 0x000c;
        let gtk = Gtk { index: 1, key: alloc::vec![0x5a; 16] };
        let mut ap = Authenticator::new(pmk(b"lab passphrase"), AA, SPA, &other.element(), &rsn, gtk);
        let mut sta = Supplicant::new(pmk(b"lab passphrase"), AA, SPA, &rsn, &rsn);
        let mut rng = Counter(0);
        let step = sta.handle(&ap.start(&mut rng), &mut rng).unwrap();
        let AuthStep::Reply(message_3) = ap.handle(&step.reply).unwrap() else { panic!("no message 3") };
        check!(sta.handle(&message_3, &mut rng).unwrap_err() == HandshakeError::RsnMismatch);
    }
}
