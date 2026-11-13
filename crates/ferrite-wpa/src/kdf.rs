//! Key derivation for WPA2-PSK: the PMK from a passphrase (PBKDF2-HMAC-SHA1) and the PTK from the
//! 802.11 PRF over the two addresses and nonces.

use core::fmt;

use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;
use snafu::{Snafu, ensure};

use crate::MacAddr;

pub type HmacSha1 = Hmac<Sha1>;

/// Iterations IEEE 802.11 fixes for the passphrase-to-PSK mapping.
const PSK_ITERATIONS: u32 = 4096;
pub const NONCE_LEN: usize = 32;
pub const KEY_LEN: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, Snafu)]
pub enum CredentialError {
    #[snafu(display("a passphrase has 8 to 63 characters, this one has {len}"))]
    PassphraseLength { len: usize },
    #[snafu(display("a passphrase is printable ASCII; byte {at} is {byte:#04x}"))]
    PassphraseByte { at: usize, byte: u8 },
    #[snafu(display("an SSID has 1 to 32 bytes, this one has {len}"))]
    SsidLength { len: usize },
}

/// A WPA2 passphrase: 8 to 63 printable ASCII characters. Its `Debug` shows only the length.
#[derive(Clone, PartialEq, Eq)]
pub struct Passphrase(alloc::vec::Vec<u8>);

impl Passphrase {
    pub fn new(bytes: &[u8]) -> Result<Self, CredentialError> {
        ensure!((8..=63).contains(&bytes.len()), PassphraseLengthSnafu { len: bytes.len() });
        if let Some(at) = bytes.iter().position(|b| !(0x20..=0x7e).contains(b)) {
            return Err(CredentialError::PassphraseByte { at, byte: bytes[at] });
        }
        Ok(Self(bytes.to_vec()))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Passphrase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Passphrase({} characters)", self.0.len())
    }
}

/// An SSID: 1 to 32 bytes, any values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ssid(alloc::vec::Vec<u8>);

impl Ssid {
    pub fn new(bytes: &[u8]) -> Result<Self, CredentialError> {
        ensure!((1..=32).contains(&bytes.len()), SsidLengthSnafu { len: bytes.len() });
        Ok(Self(bytes.to_vec()))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// The pairwise master key; for PSK, the PSK itself.
#[derive(Clone, PartialEq, Eq)]
pub struct Pmk(pub(crate) [u8; 32]);

impl Pmk {
    /// PBKDF2-HMAC-SHA1(passphrase, SSID, 4096), 256 bits.
    pub fn from_passphrase(passphrase: &Passphrase, ssid: &Ssid) -> Self {
        Self(pbkdf2::pbkdf2_hmac_array::<Sha1, 32>(&passphrase.0, &ssid.0, PSK_ITERATIONS))
    }
}

impl fmt::Debug for Pmk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Pmk(..)")
    }
}

/// The 802.11 PRF on HMAC-SHA1: blocks of HMAC(key, label || 0 || data || i), i from 0, filling `out`.
pub fn prf_sha1(key: &[u8], label: &[u8], data: &[u8], out: &mut [u8]) {
    for (i, chunk) in out.chunks_mut(20).enumerate() {
        let mut mac = <HmacSha1 as KeyInit>::new_from_slice(key).unwrap_or_else(|_| unreachable!());
        mac.update(label);
        mac.update(&[0]);
        mac.update(data);
        mac.update(&[i as u8]);
        let block = mac.finalize().into_bytes();
        chunk.copy_from_slice(&block[..chunk.len()]);
    }
}

/// The pairwise transient key for CCMP-128: KCK (MIC), KEK (key wrap) and TK (the temporal key).
#[derive(Clone, PartialEq, Eq)]
pub struct Ptk {
    pub kck: [u8; KEY_LEN],
    pub kek: [u8; KEY_LEN],
    pub tk: [u8; KEY_LEN],
}

impl fmt::Debug for Ptk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Ptk(..)")
    }
}

impl Ptk {
    /// PRF-384(PMK, "Pairwise key expansion", min(AA,SPA) || max(AA,SPA) || min(ANonce,SNonce) ||
    /// max(ANonce,SNonce)).
    pub fn derive(pmk: &Pmk, aa: MacAddr, spa: MacAddr, anonce: &[u8; NONCE_LEN], snonce: &[u8; NONCE_LEN]) -> Self {
        let mut data = [0_u8; 12 + 2 * NONCE_LEN];
        let (lo, hi) = if aa.0 <= spa.0 { (aa, spa) } else { (spa, aa) };
        data[..6].copy_from_slice(&lo.0);
        data[6..12].copy_from_slice(&hi.0);
        let (lo, hi) = if anonce <= snonce { (anonce, snonce) } else { (snonce, anonce) };
        data[12..44].copy_from_slice(lo);
        data[44..].copy_from_slice(hi);
        let mut out = [0_u8; 3 * KEY_LEN];
        prf_sha1(&pmk.0, b"Pairwise key expansion", &data, &mut out);
        let mut ptk = Self { kck: [0; KEY_LEN], kek: [0; KEY_LEN], tk: [0; KEY_LEN] };
        ptk.kck.copy_from_slice(&out[..16]);
        ptk.kek.copy_from_slice(&out[16..32]);
        ptk.tk.copy_from_slice(&out[32..]);
        ptk
    }
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};

    use super::*;

    // Vectors from hostap's src/crypto/crypto_module_tests.c (IEEE 802.11 Annex J).
    #[test]
    fn passphrases_map_to_the_annex_j_psks() {
        let cases: [(&[u8], &[u8], [u8; 32]); 3] = [
            (
                b"password",
                b"IEEE",
                [
                    0xf4, 0x2c, 0x6f, 0xc5, 0x2d, 0xf0, 0xeb, 0xef, 0x9e, 0xbb, 0x4b, 0x90, 0xb3, 0x8a, 0x5f, 0x90,
                    0x2e, 0x83, 0xfe, 0x1b, 0x13, 0x5a, 0x70, 0xe2, 0x3a, 0xed, 0x76, 0x2e, 0x97, 0x10, 0xa1, 0x2e,
                ],
            ),
            (
                b"ThisIsAPassword",
                b"ThisIsASSID",
                [
                    0x0d, 0xc0, 0xd6, 0xeb, 0x90, 0x55, 0x5e, 0xd6, 0x41, 0x97, 0x56, 0xb9, 0xa1, 0x5e, 0xc3, 0xe3,
                    0x20, 0x9b, 0x63, 0xdf, 0x70, 0x7d, 0xd5, 0x08, 0xd1, 0x45, 0x81, 0xf8, 0x98, 0x27, 0x21, 0xaf,
                ],
            ),
            (
                b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                b"ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ",
                [
                    0xbe, 0xcb, 0x93, 0x86, 0x6b, 0xb8, 0xc3, 0x83, 0x2c, 0xb7, 0x77, 0xc2, 0xf5, 0x59, 0x80, 0x7c,
                    0x8c, 0x59, 0xaf, 0xcb, 0x6e, 0xae, 0x73, 0x48, 0x85, 0x00, 0x13, 0x00, 0xa9, 0x81, 0xcc, 0x62,
                ],
            ),
        ];
        for (passphrase, ssid, psk) in cases {
            let pmk = Pmk::from_passphrase(&Passphrase::new(passphrase).unwrap(), &Ssid::new(ssid).unwrap());
            check!(pmk.0 == psk);
        }
    }

    #[test]
    fn the_prf_matches_the_published_prf_sha1_vectors() {
        let mut out = [0_u8; 64];
        prf_sha1(&[0x0b; 20], b"prefix", b"Hi There", &mut out);
        check!(
            out[..]
                == [
                    0xbc, 0xd4, 0xc6, 0x50, 0xb3, 0x0b, 0x96, 0x84, 0x95, 0x18, 0x29, 0xe0, 0xd7, 0x5f, 0x9d, 0x54,
                    0xb8, 0x62, 0x17, 0x5e, 0xd9, 0xf0, 0x06, 0x06, 0xe1, 0x7d, 0x8d, 0xa3, 0x54, 0x02, 0xff, 0xee,
                    0x75, 0xdf, 0x78, 0xc3, 0xd3, 0x1e, 0x0f, 0x88, 0x9f, 0x01, 0x21, 0x20, 0xc0, 0x86, 0x2b, 0xeb,
                    0x67, 0x75, 0x3e, 0x74, 0x39, 0xae, 0x24, 0x2e, 0xdb, 0x83, 0x73, 0x69, 0x83, 0x56, 0xcf, 0x5a,
                ][..]
        );
        prf_sha1(b"Jefe", b"prefix", b"what do ya want for nothing?", &mut out);
        check!(
            out[..]
                == [
                    0x51, 0xf4, 0xde, 0x5b, 0x33, 0xf2, 0x49, 0xad, 0xf8, 0x1a, 0xeb, 0x71, 0x3a, 0x3c, 0x20, 0xf4,
                    0xfe, 0x63, 0x14, 0x46, 0xfa, 0xbd, 0xfa, 0x58, 0x24, 0x47, 0x59, 0xae, 0x58, 0xef, 0x90, 0x09,
                    0xa9, 0x9a, 0xbf, 0x4e, 0xac, 0x2c, 0xa5, 0xfa, 0x87, 0xe6, 0x92, 0xc4, 0x40, 0xeb, 0x40, 0x02,
                    0x3e, 0x7b, 0xab, 0xb2, 0x06, 0xd6, 0x1d, 0xe7, 0xb9, 0x2f, 0x41, 0x52, 0x90, 0x92, 0xb8, 0xfc,
                ][..]
        );
        prf_sha1(&[0xaa; 20], b"prefix", &[0xdd; 50], &mut out);
        check!(
            out[..]
                == [
                    0xe1, 0xac, 0x54, 0x6e, 0xc4, 0xcb, 0x63, 0x6f, 0x99, 0x76, 0x48, 0x7b, 0xe5, 0xc8, 0x6b, 0xe1,
                    0x7a, 0x02, 0x52, 0xca, 0x5d, 0x8d, 0x8d, 0xf1, 0x2c, 0xfb, 0x04, 0x73, 0x52, 0x52, 0x49, 0xce,
                    0x9d, 0xd8, 0xd1, 0x77, 0xea, 0xd7, 0x10, 0xbc, 0x9b, 0x59, 0x05, 0x47, 0x23, 0x91, 0x07, 0xae,
                    0xf7, 0xb4, 0xab, 0xd4, 0x3d, 0x87, 0xf0, 0xa6, 0x8f, 0x1c, 0xbd, 0x9e, 0x2b, 0x6f, 0x76, 0x07,
                ][..]
        );
    }

    #[test]
    fn credentials_outside_their_ranges_are_refused() {
        assert!(let Err(CredentialError::PassphraseLength { len: 7 }) = Passphrase::new(b"1234567"));
        assert!(let Err(CredentialError::PassphraseByte { at: 8, byte: 0x0a }) = Passphrase::new(b"12345678\n"));
        assert!(let Err(CredentialError::SsidLength { len: 0 }) = Ssid::new(b""));
        check!(alloc::format!("{:?}", Passphrase::new(b"secret-words").unwrap()) == "Passphrase(12 characters)");
    }
}
