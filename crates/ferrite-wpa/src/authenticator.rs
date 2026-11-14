//! The access point side of the 4-way and group key handshakes, as hostapd runs them: for the lab's
//! access point and for testing the supplicant against an independent state machine.

use alloc::vec::Vec;

use snafu::Snafu;

use crate::eapol::{FrameError, KeyFrame, KeyInfo, wrap_key_data};
use crate::kdf::{KEY_LEN, NONCE_LEN, Pmk, Ptk};
use crate::rsn::{Akm, Gtk, Igtk, gtk_kde, igtk_kde, parse_key_data};
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
    #[snafu(display("message 2's RSNX element differs from the association request's"))]
    RsnxMismatch,
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

/// What an authenticator is told about the network and the station.
pub struct Params<'a> {
    pub akm: Akm,
    pub pmk: Pmk,
    pub aa: MacAddr,
    pub spa: MacAddr,
    /// The RSN and RSNX elements of the beacon, which message 3 carries.
    pub own_rsn: &'a [u8],
    pub own_rsnx: Option<&'a [u8]>,
    /// The RSN and RSNX elements of the station's association request, which message 2 must repeat.
    pub sta_rsn: &'a [u8],
    pub sta_rsnx: Option<&'a [u8]>,
    pub gtk: Gtk,
    /// The integrity group key; required for an AKM that requires protected management frames.
    pub igtk: Option<Igtk>,
}

pub struct Authenticator {
    akm: Akm,
    pmk: Pmk,
    aa: MacAddr,
    spa: MacAddr,
    own_elements: Vec<u8>,
    sta_rsn: Vec<u8>,
    sta_rsnx: Option<Vec<u8>>,
    gtk: Gtk,
    igtk: Option<Igtk>,
    replay: u64,
    anonce: [u8; NONCE_LEN],
    ptk: Option<Ptk>,
    state: State,
}

impl Authenticator {
    pub fn new(params: Params<'_>) -> Self {
        let mut own_elements = params.own_rsn.to_vec();
        own_elements.extend_from_slice(params.own_rsnx.unwrap_or_default());
        Self {
            akm: params.akm,
            pmk: params.pmk,
            aa: params.aa,
            spa: params.spa,
            own_elements,
            sta_rsn: params.sta_rsn.to_vec(),
            sta_rsnx: params.sta_rsnx.map(<[u8]>::to_vec),
            gtk: params.gtk,
            igtk: params.igtk,
            replay: 0,
            anonce: [0; NONCE_LEN],
            ptk: None,
            state: State::Idle,
        }
    }

    /// `info` at the descriptor version the key management defines.
    fn info(&self, info: KeyInfo) -> KeyInfo {
        KeyInfo { version: self.akm.descriptor_version(), ..info }
    }

    pub const fn replay_counter(&self) -> u64 {
        self.replay
    }

    /// Message 1 with a fresh authenticator nonce.
    pub fn start(&mut self, rng: &mut dyn Rng) -> Vec<u8> {
        rng.fill(&mut self.anonce);
        self.replay += 1;
        let mut frame = KeyFrame::new(self.info(KeyInfo { pairwise: true, ack: true, ..KeyInfo::NONE }), self.replay);
        frame.key_length = KEY_LEN as u16;
        frame.nonce = self.anonce;
        self.state = State::SentMessage1;
        frame.to_bytes()
    }

    /// A group key handshake's message 1 carrying `gtk`.
    pub fn rekey(&mut self, gtk: Gtk) -> Result<Vec<u8>, AuthenticatorError> {
        let Some(ptk) = self.ptk.clone() else { return Err(AuthenticatorError::Unexpected { info: 0 }) };
        self.replay += 1;
        let mut frame = KeyFrame::new(
            self.info(KeyInfo { ack: true, mic: true, secure: true, encrypted: true, ..KeyInfo::NONE }),
            self.replay,
        );
        let mut plain = gtk_kde(&gtk);
        if let Some(igtk) = &self.igtk {
            plain.extend_from_slice(&igtk_kde(igtk));
        }
        frame.key_data = wrap_key_data(&ptk.kek, &plain);
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
            self.info(KeyInfo {
                pairwise: true,
                install: true,
                ack: true,
                mic: true,
                secure: true,
                encrypted: true,
                ..KeyInfo::NONE
            }),
            self.replay,
        );
        frame.key_length = KEY_LEN as u16;
        frame.nonce = self.anonce;
        let mut plain = self.own_elements.clone();
        plain.extend_from_slice(&gtk_kde(&self.gtk));
        if let Some(igtk) = &self.igtk {
            plain.extend_from_slice(&igtk_kde(igtk));
        }
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
                let ptk = Ptk::derive(self.akm, &self.pmk, self.aa, self.spa, &self.anonce, &frame.nonce);
                if !KeyFrame::verify(raw, &ptk.kck) {
                    return Err(AuthenticatorError::Mic);
                }
                let data = parse_key_data(&frame.key_data);
                if data.rsn != Some(self.sta_rsn.as_slice()) {
                    return Err(AuthenticatorError::RsnMismatch);
                }
                if data.rsnx != self.sta_rsnx.as_deref() {
                    return Err(AuthenticatorError::RsnxMismatch);
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
    use crate::eapol::DescriptorVersion;
    use crate::kdf::{Passphrase, Ssid};
    use crate::rsn::{RSNX_H2E_ELEMENT, Rsn};
    use crate::supplicant::{HandshakeError, Install, Message, Params as SupplicantParams, Supplicant};

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

    /// A source that always gives the same 32 bytes.
    struct Fixed([u8; 32]);

    impl Rng for Fixed {
        fn fill(&mut self, buf: &mut [u8]) {
            buf.copy_from_slice(&self.0);
        }
    }

    const AA: MacAddr = MacAddr([2, 0, 0, 0, 1, 0]);
    const SPA: MacAddr = MacAddr([2, 0, 0, 0, 0, 0]);

    fn pmk(passphrase: &[u8]) -> Pmk {
        Pmk::from_passphrase(&Passphrase::new(passphrase).unwrap(), &Ssid::new(b"ferrite-lab").unwrap())
    }

    fn gtk() -> Gtk {
        Gtk { index: 1, key: alloc::vec![0x5a; 16] }
    }

    fn igtk() -> Igtk {
        Igtk { index: 4, ipn: [0; 6], key: [0x3c; 16] }
    }

    fn ap_params(akm: Akm, pmk: Pmk, rsn: &[u8]) -> Params<'_> {
        Params {
            akm,
            pmk,
            aa: AA,
            spa: SPA,
            own_rsn: rsn,
            own_rsnx: None,
            sta_rsn: rsn,
            sta_rsnx: None,
            gtk: gtk(),
            igtk: (akm == Akm::Sae).then(igtk),
        }
    }

    fn sta_params(akm: Akm, pmk: Pmk, rsn: &[u8]) -> SupplicantParams<'_> {
        SupplicantParams { akm, pmk, aa: AA, spa: SPA, ap_rsn: rsn, ap_rsnx: None, own_rsn: rsn, own_rsnx: None }
    }

    fn pair(sta_passphrase: &[u8]) -> (Authenticator, Supplicant) {
        let rsn = Rsn::psk_ccmp().element();
        let ap = Authenticator::new(ap_params(Akm::Psk, pmk(b"lab passphrase"), &rsn));
        (ap, Supplicant::new(sta_params(Akm::Psk, pmk(sta_passphrase), &rsn)))
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
        let rsn = Rsn::psk_ccmp().element();
        let mut forger = Authenticator::new(Params {
            gtk: Gtk { index: 1, key: alloc::vec![1; 16] },
            ..ap_params(Akm::Psk, pmk(b"lab passphrase"), &rsn)
        });
        forger.anonce = good_ap.anonce;
        forger.replay = good_ap.replay;
        forger.state = State::SentMessage1;
        let snonce = KeyFrame::parse(&step.reply).unwrap().nonce;
        let ptk = Ptk::derive(Akm::Psk, &forger.pmk, AA, SPA, &forger.anonce, &snonce);
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
        let other = other.element();
        let mut ap =
            Authenticator::new(Params { own_rsn: &other, ..ap_params(Akm::Psk, pmk(b"lab passphrase"), &rsn) });
        let mut sta = Supplicant::new(sta_params(Akm::Psk, pmk(b"lab passphrase"), &rsn));
        let mut rng = Counter(0);
        let step = sta.handle(&ap.start(&mut rng), &mut rng).unwrap();
        let AuthStep::Reply(message_3) = ap.handle(&step.reply).unwrap() else { panic!("no message 3") };
        check!(sta.handle(&message_3, &mut rng).unwrap_err() == HandshakeError::RsnMismatch);
    }

    #[test]
    fn a_message_3_whose_rsnx_element_the_beacon_did_not_have_is_refused() {
        let rsn = Rsn::sae_ccmp(None).element();
        let mut ap = Authenticator::new(Params {
            own_rsnx: Some(&RSNX_H2E_ELEMENT),
            ..ap_params(Akm::Sae, pmk(b"lab passphrase"), &rsn)
        });
        let mut sta = Supplicant::new(sta_params(Akm::Sae, pmk(b"lab passphrase"), &rsn));
        let mut rng = Counter(0);
        let step = sta.handle(&ap.start(&mut rng), &mut rng).unwrap();
        let AuthStep::Reply(message_3) = ap.handle(&step.reply).unwrap() else { panic!("no message 3") };
        check!(sta.handle(&message_3, &mut rng).unwrap_err() == HandshakeError::RsnxMismatch);
    }

    #[test]
    fn the_sae_four_way_handshake_uses_cmac_and_installs_the_igtk_beside_the_keys() {
        let rsn = Rsn::sae_ccmp(None).element();
        let mut ap = Authenticator::new(Params {
            own_rsnx: Some(&RSNX_H2E_ELEMENT),
            sta_rsnx: Some(&RSNX_H2E_ELEMENT),
            ..ap_params(Akm::Sae, pmk(b"lab passphrase"), &rsn)
        });
        let mut sta = Supplicant::new(SupplicantParams {
            ap_rsnx: Some(&RSNX_H2E_ELEMENT),
            own_rsnx: Some(&RSNX_H2E_ELEMENT),
            ..sta_params(Akm::Sae, pmk(b"lab passphrase"), &rsn)
        });
        let mut rng = Counter(0);
        let message_1 = ap.start(&mut rng);
        check!(KeyFrame::parse(&message_1).unwrap().info.version == DescriptorVersion::AkmDefined);
        let step = sta.handle(&message_1, &mut rng).unwrap();
        assert!(let Ok(AuthStep::Reply(message_3)) = ap.handle(&step.reply));
        let step = sta.handle(&message_3, &mut rng).unwrap();
        assert!(let [Install::Pairwise { tk }, Install::Group { .. }, Install::Igtk(installed)] = step.install.as_slice());
        check!(*installed == igtk());
        assert!(let Ok(AuthStep::Complete { tk: ap_tk }) = ap.handle(&step.reply));
        check!(*tk == ap_tk);

        // A rekey carries the group key and the integrity key; a repeat of the same keys installs none.
        let rekey = ap.rekey(Gtk { index: 2, key: alloc::vec![0xa5; 16] }).unwrap();
        let step = sta.handle(&rekey, &mut rng).unwrap();
        assert!(let [Install::Group { .. }] = step.install.as_slice());
        check!(ap.handle(&step.reply) == Ok(AuthStep::GroupComplete));
    }

    #[test]
    fn an_sae_message_3_without_an_igtk_is_refused_and_a_psk_frame_is_not_taken_for_sae() {
        let rsn = Rsn::sae_ccmp(None).element();
        let mut ap = Authenticator::new(Params { igtk: None, ..ap_params(Akm::Sae, pmk(b"lab passphrase"), &rsn) });
        let mut sta = Supplicant::new(sta_params(Akm::Sae, pmk(b"lab passphrase"), &rsn));
        let mut rng = Counter(0);
        let step = sta.handle(&ap.start(&mut rng), &mut rng).unwrap();
        let AuthStep::Reply(message_3) = ap.handle(&step.reply).unwrap() else { panic!("no message 3") };
        check!(sta.handle(&message_3, &mut rng).unwrap_err() == HandshakeError::NoIgtk);

        let (mut psk_ap, _) = pair(b"lab passphrase");
        assert!(let Err(HandshakeError::Frame { .. }) = sta.handle(&psk_ap.start(&mut rng), &mut rng));
    }

    /// A message 2 and message 4 for a PMK, addresses and nonces fixed by hand, computed with OpenSSL
    /// (SHA-256 KDF and AES-128-CMAC), for key management 00-0F-AC:8 with descriptor version 0.
    #[test]
    fn an_akm_8_handshake_matches_vectors_computed_with_openssl() {
        fn hex(text: &str) -> Vec<u8> {
            let digits: Vec<u8> = text.bytes().collect();
            digits.chunks(2).map(|pair| u8::from_str_radix(core::str::from_utf8(pair).unwrap(), 16).unwrap()).collect()
        }
        let pmk = Pmk(hex("4e4dfab1a2dd8ac1a91790f953faaa452ae5c6873ab75b63605ba663f8a7fe59").try_into().unwrap());
        let ap_rsn = hex("30140100000fac040100000fac040100000fac08cc00");
        let own_rsn = hex("30140100000fac040100000fac040100000fac08c000");
        let mut sta = Supplicant::new(SupplicantParams {
            akm: Akm::Sae,
            pmk,
            aa: MacAddr([2, 0, 0, 0, 0, 1]),
            spa: MacAddr([2, 0, 0, 0, 0, 2]),
            ap_rsn: &ap_rsn,
            ap_rsnx: None,
            own_rsn: &own_rsn,
            own_rsnx: None,
        });
        // The vector's replies carry 802.1X version 1 where ours carry 2, and the MIC covers that byte,
        // so the vector's MICs are verified under our KCK and the rest of each frame is compared.
        let ptk = Ptk::derive(
            Akm::Sae,
            &Pmk(hex("4e4dfab1a2dd8ac1a91790f953faaa452ae5c6873ab75b63605ba663f8a7fe59").try_into().unwrap()),
            MacAddr([2, 0, 0, 0, 0, 1]),
            MacAddr([2, 0, 0, 0, 0, 2]),
            &KeyFrame::parse(&hex(MESSAGE_1)).unwrap().nonce,
            &hex(SNONCE).try_into().unwrap(),
        );
        let same_but_mic = |ours: &[u8], vector: &[u8]| {
            let (mut ours, mut vector) = (KeyFrame::parse(ours).unwrap(), KeyFrame::parse(vector).unwrap());
            (ours.mic, vector.mic) = ([0; 16], [0; 16]);
            ours == vector
        };
        let step = sta.handle(&hex(MESSAGE_1), &mut Fixed(hex(SNONCE).try_into().unwrap())).unwrap();
        check!(KeyFrame::verify(&hex(MESSAGE_2), &ptk.kck));
        check!(same_but_mic(&step.reply, &hex(MESSAGE_2)));
        let step = sta.handle(&hex(MESSAGE_3), &mut Fixed([0; 32])).unwrap();
        check!(KeyFrame::verify(&hex(MESSAGE_4), &ptk.kck));
        check!(same_but_mic(&step.reply, &hex(MESSAGE_4)));
        assert!(let [Install::Pairwise { tk }, Install::Group { gtk, .. }, Install::Igtk(igtk)] = step.install.as_slice());
        check!(tk[..] == hex("aeb8aa4780e0bfd7497414469dd273a6")[..]);
        check!((gtk.index, igtk.index) == (1, 4));
    }

    const SNONCE: &str = "3c0eb2aac9a32be8c80969dfe8090faae17974c16d88517404f3515054fb9e8f";
    const MESSAGE_1: &str = "0203005f02008800100000000000000001000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
    const MESSAGE_2: &str = "01030075020108000000000000000000013c0eb2aac9a32be8c80969dfe8090faae17974c16d88517404f3515054fb9e8f0000000000000000000000000000000000000000000000000000000000000000cd2e7c6bd64b891381afce7a374ffbe0001630140100000fac040100000fac040100000fac08c000";
    const MESSAGE_3: &str = "020300b70213c800100000000000000002000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f000000000000000000000000000000002a0100000000000000000000000000002e1cdbd414bfd67a653140097b6e301900587e2c5119cc618e37191e5b8fdb820eca15888ebeda91865c34576dc009fd4c4036dc5360f1f982472b829a88c0c82c55d63bf32b977131efec48567dab050219b9cced4578f6b183ff15cb83c653eb5b6c24db4a699be9ed";
    const MESSAGE_4: &str = "0103005f0203080000000000000000000200000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000ca2d247cf9c1c29df7bdde82141a9f330000";
}
