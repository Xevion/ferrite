//! EAPOL-Key frames (IEEE 802.1X-2010 header, IEEE 802.11 key descriptor type 2), their MIC
//! (HMAC-SHA1-128 for descriptor version 2) and the key data's AES key wrap.

use alloc::vec::Vec;

use aes_kw::{KeyInit as _, KwAes128};
use hmac::Mac;
use snafu::{Snafu, ensure};

use crate::kdf::{HmacSha1, KEY_LEN, NONCE_LEN};

/// The Ethernet type EAPOL frames travel under.
pub const ETH_P_PAE: u16 = 0x888e;
const HEADER_LEN: usize = 4;
/// Descriptor type through key data length.
const BODY_FIXED: usize = 95;
const MIC_AT: usize = HEADER_LEN + 77;
pub const MIC_LEN: usize = 16;
const PACKET_KEY: u8 = 3;
const DESCRIPTOR_RSN: u8 = 2;
/// 802.1X-2004, which hostapd and `wpa_supplicant` send for WPA2.
const PROTOCOL_VERSION: u8 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Snafu)]
pub enum FrameError {
    #[snafu(display("an EAPOL frame of {len} bytes is shorter than its {want}"))]
    Short { len: usize, want: usize },
    #[snafu(display("EAPOL packet type {kind} is not EAPOL-Key"))]
    NotKey { kind: u8 },
    #[snafu(display("key descriptor type {kind} is not RSN (2)"))]
    Descriptor { kind: u8 },
    #[snafu(display("the key data claims {len} bytes, {left} are left"))]
    KeyDataLength { len: usize, left: usize },
    #[snafu(display("key descriptor version {version:?} is not HMAC-SHA1/AES (2)"))]
    Version { version: DescriptorVersion },
    #[snafu(display("encrypted key data of {len} bytes is not a key wrap of 8-byte blocks"))]
    WrapLength { len: usize },
    #[snafu(display("the key data failed the key wrap integrity check"))]
    Unwrap,
}

/// Key Information bits 0 to 2.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DescriptorVersion {
    /// HMAC-MD5 and RC4 (WPA1, TKIP).
    Md5Rc4,
    /// HMAC-SHA1-128 and AES key wrap.
    Sha1Aes,
    /// AES-128-CMAC and AES key wrap.
    CmacAes,
    Unknown(u8),
}

impl DescriptorVersion {
    pub const fn from_raw(raw: u8) -> Self {
        match raw {
            1 => Self::Md5Rc4,
            2 => Self::Sha1Aes,
            3 => Self::CmacAes,
            other => Self::Unknown(other),
        }
    }

    pub const fn raw(self) -> u8 {
        match self {
            Self::Md5Rc4 => 1,
            Self::Sha1Aes => 2,
            Self::CmacAes => 3,
            Self::Unknown(raw) => raw & 7,
        }
    }
}

/// The Key Information field.
#[expect(clippy::struct_excessive_bools, reason = "each is one bit of the standard field")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyInfo {
    pub version: DescriptorVersion,
    pub pairwise: bool,
    pub install: bool,
    pub ack: bool,
    pub mic: bool,
    pub secure: bool,
    pub error: bool,
    pub request: bool,
    pub encrypted: bool,
}

impl KeyInfo {
    /// Every flag clear, at descriptor version 2.
    pub const NONE: Self = Self {
        version: DescriptorVersion::Sha1Aes,
        pairwise: false,
        install: false,
        ack: false,
        mic: false,
        secure: false,
        error: false,
        request: false,
        encrypted: false,
    };

    pub fn from_raw(raw: u16) -> Self {
        let bit = |n: u16| raw & (1 << n) != 0;
        Self {
            version: DescriptorVersion::from_raw((raw & 7) as u8),
            pairwise: bit(3),
            install: bit(6),
            ack: bit(7),
            mic: bit(8),
            secure: bit(9),
            error: bit(10),
            request: bit(11),
            encrypted: bit(12),
        }
    }

    pub fn raw(self) -> u16 {
        let flag = |on: bool, n: u16| if on { 1 << n } else { 0 };
        u16::from(self.version.raw())
            | flag(self.pairwise, 3)
            | flag(self.install, 6)
            | flag(self.ack, 7)
            | flag(self.mic, 8)
            | flag(self.secure, 9)
            | flag(self.error, 10)
            | flag(self.request, 11)
            | flag(self.encrypted, 12)
    }
}

/// One EAPOL-Key frame, as parsed or to be built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyFrame {
    pub info: KeyInfo,
    pub key_length: u16,
    pub replay_counter: u64,
    pub nonce: [u8; NONCE_LEN],
    pub iv: [u8; 16],
    pub rsc: [u8; 8],
    pub mic: [u8; MIC_LEN],
    pub key_data: Vec<u8>,
}

impl KeyFrame {
    pub fn new(info: KeyInfo, replay_counter: u64) -> Self {
        Self {
            info,
            key_length: 0,
            replay_counter,
            nonce: [0; NONCE_LEN],
            iv: [0; 16],
            rsc: [0; 8],
            mic: [0; MIC_LEN],
            key_data: Vec::new(),
        }
    }

    /// Parse the frame after the Ethernet header: 802.1X header, then the key descriptor.
    pub fn parse(frame: &[u8]) -> Result<Self, FrameError> {
        ensure!(frame.len() >= HEADER_LEN + BODY_FIXED, ShortSnafu { len: frame.len(), want: HEADER_LEN + BODY_FIXED });
        ensure!(frame[1] == PACKET_KEY, NotKeySnafu { kind: frame[1] });
        let body_len = usize::from(u16::from_be_bytes([frame[2], frame[3]]));
        let body = frame
            .get(HEADER_LEN..HEADER_LEN + body_len)
            .ok_or(FrameError::Short { len: frame.len(), want: HEADER_LEN + body_len })?;
        ensure!(body.len() >= BODY_FIXED, ShortSnafu { len: frame.len(), want: HEADER_LEN + BODY_FIXED });
        ensure!(body[0] == DESCRIPTOR_RSN, DescriptorSnafu { kind: body[0] });
        let data_len = usize::from(u16::from_be_bytes([body[93], body[94]]));
        let key_data = body
            .get(BODY_FIXED..BODY_FIXED + data_len)
            .ok_or(FrameError::KeyDataLength { len: data_len, left: body.len() - BODY_FIXED })?;
        let array = |at: usize| -> [u8; 8] { body[at..at + 8].try_into().unwrap_or_default() };
        let mut frame = Self::new(KeyInfo::from_raw(u16::from_be_bytes([body[1], body[2]])), 0);
        frame.key_length = u16::from_be_bytes([body[3], body[4]]);
        frame.replay_counter = u64::from_be_bytes(array(5));
        frame.nonce.copy_from_slice(&body[13..45]);
        frame.iv.copy_from_slice(&body[45..61]);
        frame.rsc = array(61);
        frame.mic.copy_from_slice(&body[77..93]);
        frame.key_data = key_data.to_vec();
        Ok(frame)
    }

    /// The frame's bytes from the 802.1X header on, with the MIC field as it stands.
    pub fn to_bytes(&self) -> Vec<u8> {
        let body_len = BODY_FIXED + self.key_data.len();
        let mut out = Vec::with_capacity(HEADER_LEN + body_len);
        out.extend_from_slice(&[PROTOCOL_VERSION, PACKET_KEY]);
        out.extend_from_slice(&(body_len as u16).to_be_bytes());
        out.push(DESCRIPTOR_RSN);
        out.extend_from_slice(&self.info.raw().to_be_bytes());
        out.extend_from_slice(&self.key_length.to_be_bytes());
        out.extend_from_slice(&self.replay_counter.to_be_bytes());
        out.extend_from_slice(&self.nonce);
        out.extend_from_slice(&self.iv);
        out.extend_from_slice(&self.rsc);
        out.extend_from_slice(&[0; 8]);
        out.extend_from_slice(&self.mic);
        out.extend_from_slice(&(self.key_data.len() as u16).to_be_bytes());
        out.extend_from_slice(&self.key_data);
        out
    }

    /// The frame's bytes with its MIC computed under `kck`.
    pub fn sign(&mut self, kck: &[u8; KEY_LEN]) -> Vec<u8> {
        self.mic = [0; MIC_LEN];
        let mut bytes = self.to_bytes();
        let mic = mic(kck, &bytes);
        bytes[MIC_AT..MIC_AT + MIC_LEN].copy_from_slice(&mic);
        self.mic = mic;
        bytes
    }

    /// Whether the MIC of `raw`, the frame as received, verifies under `kck`; constant time.
    pub fn verify(raw: &[u8], kck: &[u8; KEY_LEN]) -> bool {
        let Some(received) = raw.get(MIC_AT..MIC_AT + MIC_LEN) else { return false };
        let mut zeroed = raw.to_vec();
        zeroed[MIC_AT..MIC_AT + MIC_LEN].fill(0);
        let mut mac = <HmacSha1 as hmac::KeyInit>::new_from_slice(kck).unwrap_or_else(|_| unreachable!());
        mac.update(&zeroed);
        mac.verify_truncated_left(received).is_ok()
    }
}

fn mic(kck: &[u8; KEY_LEN], bytes: &[u8]) -> [u8; MIC_LEN] {
    let mut mac = <HmacSha1 as hmac::KeyInit>::new_from_slice(kck).unwrap_or_else(|_| unreachable!());
    mac.update(bytes);
    let full = mac.finalize().into_bytes();
    let mut out = [0; MIC_LEN];
    out.copy_from_slice(&full[..MIC_LEN]);
    out
}

/// Unwrap encrypted key data with the KEK (RFC 3394).
pub fn unwrap_key_data(kek: &[u8; KEY_LEN], data: &[u8]) -> Result<Vec<u8>, FrameError> {
    ensure!(data.len() >= 24 && data.len().is_multiple_of(8), WrapLengthSnafu { len: data.len() });
    let kw = KwAes128::new_from_slice(kek).map_err(|_| FrameError::Unwrap)?;
    let mut out = alloc::vec![0; data.len() - 8];
    kw.unwrap_key(data, &mut out).map_err(|_| FrameError::Unwrap)?;
    Ok(out)
}

/// Pad key data as 802.11 requires (0xdd then zeroes, to a multiple of 8, at least 16) and wrap it.
pub fn wrap_key_data(kek: &[u8; KEY_LEN], plain: &[u8]) -> Vec<u8> {
    let mut padded = plain.to_vec();
    if padded.len() < 16 || !padded.len().is_multiple_of(8) {
        padded.push(0xdd);
        while padded.len() < 16 || !padded.len().is_multiple_of(8) {
            padded.push(0);
        }
    }
    let kw = KwAes128::new_from_slice(kek).unwrap_or_else(|_| unreachable!());
    let mut out = alloc::vec![0; padded.len() + 8];
    kw.wrap_key(&padded, &mut out).unwrap_or_else(|_| unreachable!());
    out
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};

    use super::*;

    #[test]
    fn key_info_bits_follow_the_standard_layout() {
        // Message 3 of the 4-way handshake as hostapd sends it: 0x13ca.
        let info = KeyInfo::from_raw(0x13ca);
        check!(info.version == DescriptorVersion::Sha1Aes);
        check!(info.pairwise && info.install && info.ack && info.mic && info.secure && info.encrypted);
        check!(!info.error && !info.request);
        check!(info.raw() == 0x13ca);
        check!(KeyInfo::from_raw(0x0005).version == DescriptorVersion::Unknown(5));
    }

    #[test]
    fn a_signed_frame_parses_back_and_a_flipped_byte_fails_its_mic() {
        let kck = [7; KEY_LEN];
        let mut frame = KeyFrame::new(KeyInfo { pairwise: true, mic: true, ..KeyInfo::NONE }, 9);
        frame.nonce = [0x42; NONCE_LEN];
        frame.key_data = alloc::vec![0x30, 2, 1, 0];
        let mut bytes = frame.sign(&kck);
        check!(KeyFrame::verify(&bytes, &kck));
        assert!(let Ok(parsed) = KeyFrame::parse(&bytes));
        check!(parsed == frame);
        bytes[20] ^= 1;
        check!(!KeyFrame::verify(&bytes, &kck));
    }

    #[test]
    fn a_key_data_length_past_the_body_is_refused() {
        let mut bytes = KeyFrame::new(KeyInfo::NONE, 0).to_bytes();
        bytes[HEADER_LEN + 94] = 10;
        assert!(let Err(FrameError::KeyDataLength { len: 10, left: 0 }) = KeyFrame::parse(&bytes));
    }

    #[test]
    fn key_data_is_padded_before_wrapping_and_a_wrong_kek_fails_the_unwrap() {
        let wrapped = wrap_key_data(&[1; KEY_LEN], &[0x30, 2, 1, 0]);
        check!(wrapped.len() == 24);
        assert!(let Ok(plain) = unwrap_key_data(&[1; KEY_LEN], &wrapped));
        check!(plain == [0x30, 2, 1, 0, 0xdd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        check!(unwrap_key_data(&[2; KEY_LEN], &wrapped) == Err(FrameError::Unwrap));
    }
}
