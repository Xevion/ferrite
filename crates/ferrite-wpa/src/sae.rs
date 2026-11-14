//! WPA3-Personal's Simultaneous Authentication of Equals (IEEE 802.11 12.4), for group 19 (NIST P-256).
//!
//! The password element is found by hash-to-element (H2E: a point `PT` per SSID and password, then a
//! scalar multiple per pair of addresses) or by hunting and pecking, which older access points still
//! need. Both sides send a commit (a scalar and an element), derive the same secret `K`, and prove it
//! with a confirm over the KCK; the PMK that comes out feeds the 4-way handshake.
//!
//! This is the protocol's arithmetic and message bodies only. The authentication frame's header
//! (algorithm 3, transaction number, status code) is the caller's, and so are retransmissions and the
//! order the frames go in; both [`Sae`] sides run the same calls.

use alloc::vec::Vec;
use core::fmt;

use hmac::{Hmac, KeyInit, Mac};
use p256::elliptic_curve::array::Array;
use p256::elliptic_curve::consts::U48;
use p256::elliptic_curve::ff::PrimeField;
use p256::elliptic_curve::group::Group as _;
use p256::elliptic_curve::ops::Reduce;
use p256::elliptic_curve::point::{AffineCoordinates, DecompressPoint};
use p256::elliptic_curve::subtle::{Choice, ConstantTimeEq as _};
use p256::hash2curve::MapToCurve;
use p256::{AffinePoint, FieldBytes, NistP256, ProjectivePoint, Scalar};
use sha2::Sha256;
use snafu::{Snafu, ensure};

use crate::kdf::{Passphrase, Pmk, Ssid, kdf_sha256};
use crate::{MacAddr, Rng};

type HmacSha256 = Hmac<Sha256>;

/// The finite cyclic group this implementation speaks: NIST P-256.
pub const GROUP: u16 = 19;
/// The authentication algorithm number SAE frames carry.
pub const AUTH_ALGORITHM: u16 = 3;
/// Authentication frame status codes SAE uses beyond success.
pub const STATUS_SUCCESS: u16 = 0;
/// An anti-clogging token is required; the body is the group and the token.
pub const STATUS_ANTI_CLOGGING_TOKEN_REQUIRED: u16 = 76;
/// The group is not supported.
pub const STATUS_UNSUPPORTED_GROUP: u16 = 77;
/// A commit whose password element came from hash-to-element.
pub const STATUS_H2E: u16 = 126;

const SCALAR_LEN: usize = 32;
const ELEMENT_LEN: usize = 64;
const COMMIT_LEN: usize = 2 + SCALAR_LEN + ELEMENT_LEN;
const HASH_LEN: usize = 32;
const PMKID_LEN: usize = 16;
/// Candidates the hunting-and-pecking loop looks at at least, so the time taken does not tell how
/// many it took; the chance of finding none in 40 is 2^-40.
const HUNT_ROUNDS: u8 = 40;
const ELEMENT_ID_EXTENSION: u8 = 255;
const ELEMENT_EXT_ANTI_CLOGGING_TOKEN: u8 = 93;

/// The order of the group `r`, less one: the modulus of the PWE's scalar.
const ORDER_MINUS_ONE: [u8; 32] = [
    0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xbc, 0xe6, 0xfa,
    0xad, 0xa7, 0x17, 0x9e, 0x84, 0xf3, 0xb9, 0xca, 0xc2, 0xfc, 0x63, 0x25, 0x50,
];

#[derive(Debug, Clone, PartialEq, Eq, Snafu)]
pub enum SaeError {
    #[snafu(display("the commit message is {len} bytes, shorter than the {COMMIT_LEN} it needs"))]
    Truncated { len: usize },
    #[snafu(display("the commit message names group {group}, and only group {GROUP} is supported"))]
    UnsupportedGroup { group: u16 },
    #[snafu(display("the peer's commit scalar is outside 2 to r-1"))]
    Scalar,
    #[snafu(display("the peer's commit element is not a point of the curve"))]
    Element,
    #[snafu(display("the peer's commit repeats ours (a reflection attack)"))]
    Reflection,
    #[snafu(display("the shared secret is the point at infinity"))]
    Infinity,
    #[snafu(display("the confirm message does not verify (another password, or a forged frame)"))]
    Confirm,
    #[snafu(display("the confirm message is {len} bytes, not {}", 2 + HASH_LEN))]
    ConfirmLength { len: usize },
    #[snafu(display("no peer commit has been processed yet"))]
    NoPeer,
}

/// How the password element is derived; a network says which through its RSNX element and its
/// membership selectors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    HuntAndPeck,
    HashToElement,
}

impl Method {
    /// The status code a commit of this method carries in its authentication frame.
    pub const fn commit_status(self) -> u16 {
        match self {
            Self::HuntAndPeck => STATUS_SUCCESS,
            Self::HashToElement => STATUS_H2E,
        }
    }
}

/// The password element, a point nobody without the password and both addresses can compute.
#[derive(Clone, PartialEq, Eq)]
pub struct Pwe(AffinePoint);

impl fmt::Debug for Pwe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Pwe(..)")
    }
}

/// HKDF-Extract (RFC 5869) with HMAC-SHA-256: the salt keys the HMAC and the input is its message.
fn hkdf_extract(salt: &[u8], ikm: &[u8]) -> [u8; HASH_LEN] {
    let mut mac = <HmacSha256 as KeyInit>::new_from_slice(salt).unwrap_or_else(|_| unreachable!());
    mac.update(ikm);
    mac.finalize().into_bytes().into()
}

/// HKDF-Expand (RFC 5869) with HMAC-SHA-256 for at most two blocks: `out` is 33 to 64 bytes.
fn hkdf_expand_two_blocks(prk: &[u8; HASH_LEN], info: &[u8], out: &mut [u8; 48]) {
    let block = |previous: &[u8], counter: u8| -> [u8; HASH_LEN] {
        let mut mac = <HmacSha256 as KeyInit>::new_from_slice(prk).unwrap_or_else(|_| unreachable!());
        mac.update(previous);
        mac.update(info);
        mac.update(&[counter]);
        mac.finalize().into_bytes().into()
    };
    let first = block(&[], 1);
    let second = block(&first, 2);
    out[..HASH_LEN].copy_from_slice(&first);
    out[HASH_LEN..].copy_from_slice(&second[..48 - HASH_LEN]);
}

/// The addresses as the PWE derivations order them: larger first.
fn max_min(a: MacAddr, b: MacAddr) -> [u8; 12] {
    let (hi, lo) = if a.0 > b.0 { (a, b) } else { (b, a) };
    let mut out = [0; 12];
    out[..6].copy_from_slice(&hi.0);
    out[6..].copy_from_slice(&lo.0);
    out
}

impl Pwe {
    /// Hunting and pecking (12.4.4.2.2): candidate x coordinates from the password, the addresses and
    /// a counter, until one is on the curve; the first found is kept, and the loop runs on to
    /// [`HUNT_ROUNDS`] so its duration does not show where it was found.
    pub fn hunt_and_peck(password: &Passphrase, a: MacAddr, b: MacAddr) -> Self {
        let key = max_min(a, b);
        let mut prime = [0; 32];
        prime.copy_from_slice(&P_BYTES);
        let mut found: Option<AffinePoint> = None;
        let mut counter = 1_u8;
        while counter <= HUNT_ROUNDS || found.is_none() {
            let mut seed = <HmacSha256 as KeyInit>::new_from_slice(&key).unwrap_or_else(|_| unreachable!());
            seed.update(password.as_bytes());
            seed.update(&[counter]);
            let seed: [u8; HASH_LEN] = seed.finalize().into_bytes().into();
            let candidate = kdf_sha256(&seed, b"SAE Hunting and Pecking", &prime, 256);
            let x = FieldBytes::try_from(candidate.as_slice()).unwrap_or_default();
            // Decompression fails for x >= p and for a non-residue alike, which the standard also rejects.
            let point: Option<AffinePoint> =
                Option::from(AffinePoint::decompress(&x, Choice::from(seed[HASH_LEN - 1] & 1)));
            if found.is_none() {
                found = point;
            }
            counter = counter.saturating_add(1);
        }
        Self(found.unwrap_or_default())
    }
}

/// The prime of the field, big-endian: the context of the hunting-and-pecking KDF.
const P_BYTES: [u8; 32] = [
    0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
];

/// The point `PT` of hash-to-element (12.4.4.2.3), fixed by the SSID and the password: a pair of
/// addresses turns it into a [`Pwe`] with a scalar multiplication, with no search.
#[derive(Clone, PartialEq, Eq)]
pub struct Pt(AffinePoint);

impl fmt::Debug for Pt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Pt(..)")
    }
}

/// SSWU on `u` (RFC 9380, the P-256 suite SAE names), from the 48 bytes an HKDF-Expand gave.
fn sswu(okm: &[u8; 48]) -> ProjectivePoint {
    let u = <<NistP256 as MapToCurve>::FieldElement as Reduce<Array<u8, U48>>>::reduce(&Array::from(*okm));
    NistP256::map_to_curve(u)
}

impl Pt {
    /// `PT = SSWU(u1) + SSWU(u2)`, `u1` and `u2` from HKDF over `H(SSID, password [|| identifier])`.
    pub fn derive(ssid: &Ssid, password: &Passphrase, identifier: Option<&[u8]>) -> Self {
        let mut ikm = password.as_bytes().to_vec();
        ikm.extend_from_slice(identifier.unwrap_or_default());
        let seed = hkdf_extract(ssid.as_bytes(), &ikm);
        let mut first = [0; 48];
        let mut second = [0; 48];
        hkdf_expand_two_blocks(&seed, b"SAE Hash to Element u1 P1", &mut first);
        hkdf_expand_two_blocks(&seed, b"SAE Hash to Element u2 P2", &mut second);
        Self((sswu(&first) + sswu(&second)).to_affine())
    }

    /// `PWE = PT` times `H(0^32, MAX(a, b) || MIN(a, b))` reduced to 1 through r-1.
    pub fn pwe(&self, a: MacAddr, b: MacAddr) -> Pwe {
        let mut val = hkdf_extract(&[0; HASH_LEN], &max_min(a, b));
        // The hash is below 2^256 and r-1 is above 2^255, so a single subtraction is the reduction.
        if val >= ORDER_MINUS_ONE {
            let mut borrow = 0_i16;
            for (v, m) in val.iter_mut().zip(ORDER_MINUS_ONE).rev() {
                let diff = i16::from(*v) - i16::from(m) - borrow;
                borrow = i16::from(diff < 0);
                *v = diff.rem_euclid(256) as u8;
            }
        }
        for byte in val.iter_mut().rev() {
            let (sum, carry) = byte.overflowing_add(1);
            *byte = sum;
            if !carry {
                break;
            }
        }
        let scalar = Option::<Scalar>::from(Scalar::from_repr(FieldBytes::from(val))).unwrap_or(Scalar::ONE);
        Pwe((ProjectivePoint::from(self.0) * scalar).to_affine())
    }
}

/// A commit message: the group, a scalar and an element.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Commit {
    scalar: Scalar,
    element: AffinePoint,
}

impl Commit {
    fn element_bytes(&self) -> [u8; ELEMENT_LEN] {
        let mut out = [0; ELEMENT_LEN];
        out[..32].copy_from_slice(&self.element.x());
        out[32..].copy_from_slice(&self.element.y());
        out
    }

    fn scalar_bytes(&self) -> [u8; SCALAR_LEN] {
        self.scalar.to_repr().into()
    }

    /// The scalar and element after the group and any token: `None` for a scalar outside 2 to r-1
    /// (`Err(Scalar)`) or an element off the curve.
    fn parse(body: &[u8]) -> Result<Self, SaeError> {
        ensure!(body.len() >= COMMIT_LEN, TruncatedSnafu { len: body.len() });
        let group = u16::from_le_bytes([body[0], body[1]]);
        ensure!(group == GROUP, UnsupportedGroupSnafu { group });
        let scalar = Option::<Scalar>::from(Scalar::from_repr(FieldBytes::try_from(&body[2..34]).unwrap_or_default()))
            .filter(|s| *s != Scalar::ZERO && *s != Scalar::ONE)
            .ok_or(SaeError::Scalar)?;
        let x = FieldBytes::try_from(&body[34..66]).unwrap_or_default();
        let y = FieldBytes::try_from(&body[66..98]).unwrap_or_default();
        let element = Option::<AffinePoint>::from(AffinePoint::from_coordinates(&x, &y)).ok_or(SaeError::Element)?;
        Ok(Self { scalar, element })
    }
}

/// What the commit exchange derived: the key confirmation key, the PMK and the PMKID.
struct Keys {
    kck: [u8; HASH_LEN],
    pmk: Pmk,
    pmkid: [u8; PMKID_LEN],
}

/// One side of an SAE exchange.
pub struct Sae {
    method: Method,
    pwe: Pwe,
    rand: Scalar,
    own: Commit,
    peer: Option<Commit>,
    keys: Option<Keys>,
    send_confirm: u16,
}

impl fmt::Debug for Sae {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Sae(..)")
    }
}

/// A scalar in 1 to r-1 from the random source.
fn random_scalar(rng: &mut dyn Rng) -> Scalar {
    loop {
        let mut bytes = [0_u8; 48];
        rng.fill(&mut bytes);
        let scalar = <Scalar as Reduce<Array<u8, U48>>>::reduce(&Array::from(bytes));
        if scalar != Scalar::ZERO {
            return scalar;
        }
    }
}

impl Sae {
    /// Start an exchange over `pwe`: draw `rand` and `mask`, and compute the commit scalar
    /// `rand + mask` and element `-(mask * PWE)`. The sum must not be 0 or 1.
    pub fn new(method: Method, pwe: Pwe, rng: &mut dyn Rng) -> Self {
        loop {
            let rand = random_scalar(rng);
            let mask = random_scalar(rng);
            let scalar = rand + mask;
            if scalar == Scalar::ZERO || scalar == Scalar::ONE {
                continue;
            }
            let element = (-(ProjectivePoint::from(pwe.0) * mask)).to_affine();
            return Self {
                method,
                pwe,
                rand,
                own: Commit { scalar, element },
                peer: None,
                keys: None,
                send_confirm: 0,
            };
        }
    }

    pub const fn method(&self) -> Method {
        self.method
    }

    /// The body of a commit authentication frame: the group, an anti-clogging `token` the access point
    /// asked for (before the scalar for hunting and pecking, in an extension element after the commit
    /// for hash-to-element), the scalar and the element.
    pub fn commit_body(&self, token: Option<&[u8]>) -> Vec<u8> {
        let mut body = Vec::with_capacity(COMMIT_LEN + token.map_or(0, <[u8]>::len) + 3);
        body.extend_from_slice(&GROUP.to_le_bytes());
        if let (Some(token), Method::HuntAndPeck) = (token, self.method) {
            body.extend_from_slice(token);
        }
        body.extend_from_slice(&self.own.scalar_bytes());
        body.extend_from_slice(&self.own.element_bytes());
        if let (Some(token), Method::HashToElement) = (token, self.method) {
            body.push(ELEMENT_ID_EXTENSION);
            body.push(1 + token.len() as u8);
            body.push(ELEMENT_EXT_ANTI_CLOGGING_TOKEN);
            body.extend_from_slice(token);
        }
        body
    }

    /// Take the peer's commit (the frame body, as [`Sae::commit_body`] writes it) and derive the keys.
    /// A commit that repeats ours is a reflection and fails with [`SaeError::Reflection`]; the caller
    /// drops such a frame without answering.
    pub fn process_commit(&mut self, body: &[u8]) -> Result<(), SaeError> {
        let peer = Commit::parse(body)?;
        ensure!(peer != self.own, ReflectionSnafu);
        // K = rand * (peer-scalar * PWE + PEER-ELEMENT)
        let shared =
            (ProjectivePoint::from(self.pwe.0) * peer.scalar + ProjectivePoint::from(peer.element)) * self.rand;
        ensure!(!bool::from(shared.is_identity()), InfinitySnafu);
        let k = shared.to_affine().x();
        let keyseed = hkdf_extract(&[0; HASH_LEN], &k);
        let sum: [u8; SCALAR_LEN] = (self.own.scalar + peer.scalar).to_repr().into();
        let derived = kdf_sha256(&keyseed, b"SAE KCK and PMK", &sum, 512);
        let mut keys = Keys { kck: [0; HASH_LEN], pmk: Pmk([0; 32]), pmkid: [0; PMKID_LEN] };
        keys.kck.copy_from_slice(&derived[..32]);
        keys.pmk.0.copy_from_slice(&derived[32..]);
        keys.pmkid.copy_from_slice(&sum[..PMKID_LEN]);
        self.peer = Some(peer);
        self.keys = Some(keys);
        Ok(())
    }

    /// The `CN` function: `HMAC(KCK, send-confirm || scalar1 || element1 || scalar2 || element2)`.
    fn confirm_value(kck: &[u8; HASH_LEN], send_confirm: u16, first: &Commit, second: &Commit) -> [u8; HASH_LEN] {
        let mut mac = <HmacSha256 as KeyInit>::new_from_slice(kck).unwrap_or_else(|_| unreachable!());
        mac.update(&send_confirm.to_le_bytes());
        mac.update(&first.scalar_bytes());
        mac.update(&first.element_bytes());
        mac.update(&second.scalar_bytes());
        mac.update(&second.element_bytes());
        mac.finalize().into_bytes().into()
    }

    /// The body of a confirm authentication frame, with a send-confirm counter one higher than the
    /// last one sent.
    pub fn confirm_body(&mut self) -> Result<Vec<u8>, SaeError> {
        let (Some(peer), Some(keys)) = (&self.peer, &self.keys) else { return Err(SaeError::NoPeer) };
        self.send_confirm = self.send_confirm.saturating_add(1);
        let mut body = Vec::with_capacity(2 + HASH_LEN);
        body.extend_from_slice(&self.send_confirm.to_le_bytes());
        body.extend_from_slice(&Self::confirm_value(&keys.kck, self.send_confirm, &self.own, peer));
        Ok(body)
    }

    /// Verify the peer's confirm: `CN(KCK, peer-send-confirm, peer commit, own commit)`, in constant
    /// time.
    pub fn check_confirm(&self, body: &[u8]) -> Result<(), SaeError> {
        let (Some(peer), Some(keys)) = (&self.peer, &self.keys) else { return Err(SaeError::NoPeer) };
        ensure!(body.len() >= 2 + HASH_LEN, ConfirmLengthSnafu { len: body.len() });
        let send_confirm = u16::from_le_bytes([body[0], body[1]]);
        let expected = Self::confirm_value(&keys.kck, send_confirm, peer, &self.own);
        if bool::from(expected.ct_eq(&body[2..2 + HASH_LEN])) { Ok(()) } else { Err(SaeError::Confirm) }
    }

    /// The PMK, once the peer's commit has been processed.
    pub fn pmk(&self) -> Option<&Pmk> {
        self.keys.as_ref().map(|keys| &keys.pmk)
    }

    /// The PMKID a (re)association request names, once the peer's commit has been processed.
    pub fn pmkid(&self) -> Option<[u8; PMKID_LEN]> {
        self.keys.as_ref().map(|keys| keys.pmkid)
    }
}

/// An authentication frame's fixed fields and body, from what follows the 802.11 header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuthFrame<'a> {
    pub algorithm: u16,
    pub transaction: u16,
    pub status: u16,
    pub body: &'a [u8],
}

impl<'a> AuthFrame<'a> {
    pub fn parse(after_header: &'a [u8]) -> Option<Self> {
        let [a0, a1, t0, t1, s0, s1, body @ ..] = after_header else { return None };
        Some(Self {
            algorithm: u16::from_le_bytes([*a0, *a1]),
            transaction: u16::from_le_bytes([*t0, *t1]),
            status: u16::from_le_bytes([*s0, *s1]),
            body,
        })
    }
}

/// An anti-clogging token request's token: the body after the group.
pub fn anti_clogging_token(body: &[u8]) -> Option<&[u8]> {
    body.get(2..).filter(|token| !token.is_empty())
}

#[cfg(test)]
mod tests {
    use alloc::string::String;

    use assert2::{assert, check};
    use rstest::{fixture, rstest};

    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        let digits: Vec<u8> = text.bytes().filter(u8::is_ascii_hexdigit).collect();
        digits.chunks(2).map(|pair| u8::from_str_radix(core::str::from_utf8(pair).unwrap(), 16).unwrap()).collect()
    }

    fn encode(bytes: &[u8]) -> String {
        use core::fmt::Write as _;
        bytes.iter().fold(String::new(), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
    }

    fn xy(point: &AffinePoint) -> (String, String) {
        (encode(&point.x()), encode(&point.y()))
    }

    // IEEE 802.11-2020 J.10, the example with a password, an SSID and a password identifier.
    const SSID: &[u8] = b"byteme";
    const PASSWORD: &[u8] = b"mekmitasdigoat";
    const IDENTIFIER: &[u8] = b"psk4internet";

    #[fixture]
    fn passphrase() -> Passphrase {
        Passphrase::new(PASSWORD).unwrap()
    }

    #[test]
    fn the_order_and_prime_constants_are_the_curves() {
        // r-1 reduces to itself as a scalar and r does not, so the bytes are r-1 exactly.
        check!(Option::<Scalar>::from(Scalar::from_repr(FieldBytes::from(ORDER_MINUS_ONE))).is_some());
        let mut order = ORDER_MINUS_ONE;
        order[31] += 1;
        check!(Option::<Scalar>::from(Scalar::from_repr(FieldBytes::from(order))).is_none());
        // p-1 is on the field side of the same check: x = p-1 decompresses or not, x = p never does.
        check!(
            Option::<AffinePoint>::from(AffinePoint::decompress(&FieldBytes::from(P_BYTES), Choice::from(0))).is_none()
        );
    }

    /// Counters 1, 2 and 40 of the 802.11 hunting-and-pecking example: seed in, candidate x out.
    #[rstest]
    #[case::counter_1(
        "a9025368ef78f7d65e8d4d556f0d1d0d758f2f7f1e116eb1d11307a7e8a9621a",
        "b8e89a725c57f18e8f68a7f72613e15f1c904938c38800efa01f1306f5e454b5"
    )]
    #[case::counter_2(
        "954bbbf8923284e4ca164e3af0b9520ce53aa35be39020e9ccb23aff86df2226",
        "da6eb7b06a1ac5624974f90afdd6a8e9d5722634cf987c34defc91a9874e5658"
    )]
    #[case::counter_40(
        "cde7b81eb539c87af5bf1be2402d315c45ad4c3db06c9c56b7f8b7daae5e5842",
        "2e12a1d615647963fd7aa4a905fd51b6f49a902fd917ef8f0ff200102699ecdb"
    )]
    fn kdf_hash_length_gives_the_candidates_of_the_hunting_and_pecking_example(
        #[case] seed: &str,
        #[case] candidate: &str,
    ) {
        check!(encode(&kdf_sha256(&hex(seed), b"SAE Hunting and Pecking", &P_BYTES, 256)) == candidate);
    }

    #[rstest]
    #[case::a_first(MacAddr([0x4d, 0x3f, 0x2f, 0xff, 0xe3, 0x87]), MacAddr([0xa5, 0xd8, 0xaa, 0x95, 0x8e, 0x3c]))]
    #[case::b_first(MacAddr([0xa5, 0xd8, 0xaa, 0x95, 0x8e, 0x3c]), MacAddr([0x4d, 0x3f, 0x2f, 0xff, 0xe3, 0x87]))]
    fn hunting_and_pecking_finds_the_password_element_of_the_standard_in_either_address_order(
        passphrase: Passphrase,
        #[case] first: MacAddr,
        #[case] second: MacAddr,
    ) {
        let (x, y) = xy(&Pwe::hunt_and_peck(&passphrase, first, second).0);
        check!(x == "da6eb7b06a1ac5624974f90afdd6a8e9d5722634cf987c34defc91a9874e5658");
        check!(y == "f4fefd130bd5be08fe68af3e4a290272ec065fd3671f3c25bf8ec419ddc9b822");
    }

    #[test]
    fn hash_to_element_matches_the_standards_intermediate_values() {
        let seed = hkdf_extract(SSID, &[PASSWORD, IDENTIFIER].concat());
        check!(encode(&seed) == "3bd53fe9223dc0280fbfce17d7a3564064e20f48c6ec72246ce367b5569a22af");
        let (mut u1, mut u2) = ([0; 48], [0; 48]);
        hkdf_expand_two_blocks(&seed, b"SAE Hash to Element u1 P1", &mut u1);
        hkdf_expand_two_blocks(&seed, b"SAE Hash to Element u2 P2", &mut u2);
        check!(
            encode(&u1)
                == "a5044469ab16f25b6abf1e0e37a36b56f50be73369053df8db87989a6b66fd1a491f1cdacbd07931620f83008ffc0ecc"
        );
        check!(
            encode(&u2)
                == "9b4e0d5b1879f253c5319615099b05aec5b06fa5e788bcfd1e9ea60d33436927190814c322a62585c93c577bbaa3d307"
        );
        let (x1, y1) = xy(&sswu(&u1).to_affine());
        check!(x1 == "a07c260764a13445ff8cd97c5acc644e7119bde51bad42583eed6f4109639e6b");
        check!(y1 == "3bdc8df0d32337936c74df604933a454142251c53c576c0351b28deaf9428d7e");
        let (x2, y2) = xy(&sswu(&u2).to_affine());
        check!(x2 == "72cd2a967a837fea5051f0133db46227775ba09f7b6dfb99ae7a8ef22c7d34a0");
        check!(y2 == "864390d797d352b368d311af515bde116fe54459fec867ee18a8a1619ca3ff59");
    }

    #[rstest]
    fn hash_to_element_derives_the_standards_pt(passphrase: Passphrase) {
        let pt = Pt::derive(&Ssid::new(SSID).unwrap(), &passphrase, Some(IDENTIFIER));
        let (x, y) = xy(&pt.0);
        check!(x == "b6e38c98750c684b5d17c3d8c9a4100b39931279187ca6cced5f37ef46ddfa97");
        check!(y == "5687e972e50f73e3898861e7edad21bea7d5f622df88243bb804920ae8e647fa");
    }

    #[rstest]
    #[case::a_first(MacAddr([0x00, 0x09, 0x5b, 0x66, 0xec, 0x1e]), MacAddr([0x00, 0x0b, 0x6b, 0xd9, 0x02, 0x46]))]
    #[case::b_first(MacAddr([0x00, 0x0b, 0x6b, 0xd9, 0x02, 0x46]), MacAddr([0x00, 0x09, 0x5b, 0x66, 0xec, 0x1e]))]
    fn hash_to_element_derives_the_standards_pwe_from_pt_in_either_address_order(
        passphrase: Passphrase,
        #[case] first: MacAddr,
        #[case] second: MacAddr,
    ) {
        let pt = Pt::derive(&Ssid::new(SSID).unwrap(), &passphrase, Some(IDENTIFIER));
        let (x, y) = xy(&pt.pwe(first, second).0);
        check!(x == "c93049b9e64000f848201649e999f2b5c22dea69b5632c9df4d633b8aa1f6c1e");
        check!(y == "73634e94b53d82e7383a8d258199d9dc1a5ee8269d060382ccbf33e614ff59a0");
    }

    // The commit and confirm messages of the same example (IEEE 802.11-18/1104r0, "New Test Vectors for
    // SAE"): both commits, the keyseed, the KCK and PMK the KDF gives, and both confirms.
    const LOCAL_COMMIT: &str = "1300eb3bab1964e4a0ab05925ddf3339519138bc65d6cdc0f813dd6fd4344eb4\
        bfe44b5c21597658f4e3eddfb4b99f25b4d6540f32ff1fd5c530c60a794448610bc6de3d92bdbbd47d935980ca6cf8988ab6630be6\
        764c885ceb9793970f695217eeff0d2170736b34696e7465726e6574";
    const PEER_COMMIT: &str = "13005564f045b2ea1e566cf1dd741f70d9be35d2df5b9a5502946ee03cf8dae2\
        7e1e05b8430eb7a99e24877ce69baf3dc580e309633d6b385f83ee1c3ec3591f1a5393c06e805ddceb2fde50930dd7cfebb987c6ff\
        9666af164eb5184d8e6662ed6aff0d2170736b34696e7465726e6574";
    const KEYSEED: &str = "7457a00754dcc4e3dc2850c124d6bb8fa1699d7fa33bb0667d9c34eeb513deb9";
    const KCK: &str = "599d6f1e27548be8499dceed2feccf94818ce1c79f1b4eb3d6a53228a09bf3ed";
    const PMK: &str = "7aead86fba4c3221fc437f5f14d70d854ea5d5aac1690116793081eda4d557c5";
    const LOCAL_CONFIRM: &str = "010012d9d5c78c500526d36c41dbc56aedf2914cedddd7cad4a58c48f83dbde9fc77";
    const PEER_CONFIRM: &str = "010002871cf906898b8060ec184143be77b8c08a8019b13eb6d0aef0d8383dfac2fd";

    /// A commit of the example with no password: its point is an arbitrary one, as the exchange's
    /// state would hold it, because the keys here are derived from the published keyseed.
    fn commit_of(text: &str) -> Commit {
        Commit::parse(&hex(text)).expect("a commit of the example parses")
    }

    #[test]
    fn the_example_commits_parse_past_their_password_identifier_element() {
        let local = commit_of(LOCAL_COMMIT);
        let peer = commit_of(PEER_COMMIT);
        check!(local.scalar_bytes() == hex(LOCAL_COMMIT)[2..34]);
        check!(peer.element_bytes() == hex(PEER_COMMIT)[34..98]);
    }

    #[test]
    fn the_keyseed_gives_the_standards_kck_and_pmk_over_the_sum_of_the_scalars() {
        let (local, peer) = (commit_of(LOCAL_COMMIT), commit_of(PEER_COMMIT));
        let sum: [u8; 32] = (local.scalar + peer.scalar).to_repr().into();
        let derived = kdf_sha256(&hex(KEYSEED), b"SAE KCK and PMK", &sum, 512);
        check!(encode(&derived[..32]) == KCK);
        check!(encode(&derived[32..]) == PMK);
    }

    #[test]
    fn confirms_are_the_hmac_of_the_counter_and_both_commits_in_the_order_the_sender_sees_them() {
        let (local, peer) = (commit_of(LOCAL_COMMIT), commit_of(PEER_COMMIT));
        let kck: [u8; 32] = hex(KCK).try_into().unwrap();
        let (local_confirm, peer_confirm) = (hex(LOCAL_CONFIRM), hex(PEER_CONFIRM));
        check!(Sae::confirm_value(&kck, 1, &local, &peer)[..] == local_confirm[2..]);
        check!(Sae::confirm_value(&kck, 1, &peer, &local)[..] == peer_confirm[2..]);
    }

    /// Counter bytes, so a run's randomness is fixed.
    struct Counter(u8);

    impl Rng for Counter {
        fn fill(&mut self, buf: &mut [u8]) {
            for byte in buf {
                self.0 = self.0.wrapping_add(37);
                *byte = self.0;
            }
        }
    }

    const STA: MacAddr = MacAddr([2, 0, 0, 0, 0, 0]);
    const AP: MacAddr = MacAddr([2, 0, 0, 0, 1, 0]);

    fn pair(method: Method, sta_password: &[u8], ap_password: &[u8]) -> (Sae, Sae) {
        let ssid = Ssid::new(b"ferrite-lab").unwrap();
        let derive = |password: &[u8], me: MacAddr, you: MacAddr, seed: u8| {
            let password = Passphrase::new(password).unwrap();
            let pwe = match method {
                Method::HuntAndPeck => Pwe::hunt_and_peck(&password, me, you),
                Method::HashToElement => Pt::derive(&ssid, &password, None).pwe(me, you),
            };
            Sae::new(method, pwe, &mut Counter(seed))
        };
        (derive(sta_password, STA, AP, 1), derive(ap_password, AP, STA, 101))
    }

    /// The exchange as an access point runs it: commit from the station, commit and confirm back, the
    /// station's confirm, and both PMKs agree.
    fn run(sta: &mut Sae, ap: &mut Sae) -> Result<(), SaeError> {
        ap.process_commit(&sta.commit_body(None))?;
        sta.process_commit(&ap.commit_body(None))?;
        let ap_confirm = ap.confirm_body()?;
        let sta_confirm = sta.confirm_body()?;
        sta.check_confirm(&ap_confirm)?;
        ap.check_confirm(&sta_confirm)
    }

    #[rstest]
    #[case::hunt_and_peck(Method::HuntAndPeck)]
    #[case::hash_to_element(Method::HashToElement)]
    fn both_methods_agree_on_the_pmk_and_pmkid(#[case] method: Method) {
        let (mut sta, mut ap) = pair(method, b"lab passphrase", b"lab passphrase");
        assert!(let Ok(()) = run(&mut sta, &mut ap));
        check!(sta.pmk().unwrap() == ap.pmk().unwrap());
        check!(sta.pmkid() == ap.pmkid());
        check!(sta.pmkid().is_some());
    }

    #[test]
    fn another_password_fails_the_confirm_and_the_methods_do_not_mix() {
        let (mut sta, mut ap) = pair(Method::HashToElement, b"lab passphrase", b"other passphrase");
        check!(run(&mut sta, &mut ap) == Err(SaeError::Confirm));
        let (hunt, _) = pair(Method::HuntAndPeck, b"lab passphrase", b"lab passphrase");
        let (_, mut h2e_ap) = pair(Method::HashToElement, b"lab passphrase", b"lab passphrase");
        h2e_ap.process_commit(&hunt.commit_body(None)).unwrap();
        let (mut hunt_sta, _) = pair(Method::HuntAndPeck, b"lab passphrase", b"lab passphrase");
        hunt_sta.process_commit(&h2e_ap.commit_body(None)).unwrap();
        check!(hunt_sta.check_confirm(&h2e_ap.confirm_body().unwrap()) == Err(SaeError::Confirm));
    }

    #[test]
    fn a_reflected_commit_is_refused_without_deriving_keys() {
        let (mut sta, _) = pair(Method::HashToElement, b"lab passphrase", b"lab passphrase");
        let own = sta.commit_body(None);
        check!(sta.process_commit(&own) == Err(SaeError::Reflection));
        check!(sta.pmk().is_none());
        check!(sta.confirm_body() == Err(SaeError::NoPeer));
    }

    #[rstest]
    #[case::truncated(|body: &mut Vec<u8>| body.truncate(97), SaeError::Truncated { len: 97 })]
    #[case::other_group(|body: &mut Vec<u8>| body[0] = 20, SaeError::UnsupportedGroup { group: 20 })]
    #[case::scalar_one(|body: &mut Vec<u8>| { body[2..34].fill(0); body[33] = 1; }, SaeError::Scalar)]
    #[case::scalar_zero(|body: &mut Vec<u8>| body[2..34].fill(0), SaeError::Scalar)]
    #[case::scalar_not_below_r(|body: &mut Vec<u8>| body[2..34].fill(0xff), SaeError::Scalar)]
    #[case::element_off_the_curve(|body: &mut Vec<u8>| body[97] ^= 1, SaeError::Element)]
    fn a_malformed_commit_is_refused(#[case] damage: fn(&mut Vec<u8>), #[case] want: SaeError) {
        let (mut sta, ap) = pair(Method::HashToElement, b"lab passphrase", b"lab passphrase");
        let mut body = ap.commit_body(None);
        damage(&mut body);
        check!(sta.process_commit(&body) == Err(want));
        check!(sta.pmk().is_none());
    }

    #[test]
    fn a_tampered_confirm_and_a_short_one_are_refused() {
        let (mut sta, mut ap) = pair(Method::HuntAndPeck, b"lab passphrase", b"lab passphrase");
        ap.process_commit(&sta.commit_body(None)).unwrap();
        sta.process_commit(&ap.commit_body(None)).unwrap();
        let mut confirm = ap.confirm_body().unwrap();
        check!(sta.check_confirm(&confirm).is_ok());
        confirm[10] ^= 1;
        check!(sta.check_confirm(&confirm) == Err(SaeError::Confirm));
        check!(sta.check_confirm(&confirm[..20]) == Err(SaeError::ConfirmLength { len: 20 }));
    }

    #[rstest]
    #[case::hunt_and_peck(Method::HuntAndPeck, 2..7, COMMIT_LEN + 5)]
    #[case::hash_to_element(Method::HashToElement, COMMIT_LEN + 3..COMMIT_LEN + 8, COMMIT_LEN + 8)]
    fn a_token_rides_before_the_scalar_for_hunting_and_pecking_and_after_the_commit_for_h2e(
        #[case] method: Method,
        #[case] token_at: core::ops::Range<usize>,
        #[case] len: usize,
    ) {
        let (sae, _) = pair(method, b"lab passphrase", b"lab passphrase");
        let body = sae.commit_body(Some(&[7; 5]));
        check!(body.len() == len);
        check!(body[token_at] == [7; 5]);
        if method == Method::HashToElement {
            check!(body[COMMIT_LEN..COMMIT_LEN + 3] == [255, 6, 93]);
        }
    }

    #[rstest]
    #[case::token(&[19, 0, 1, 2], Some(&[1, 2][..]))]
    #[case::group_only(&[19, 0], None)]
    fn a_token_request_carries_its_token_after_the_group(#[case] body: &[u8], #[case] want: Option<&[u8]>) {
        check!(anti_clogging_token(body) == want);
    }

    #[test]
    fn an_authentication_frame_splits_into_its_fixed_fields_and_body() {
        let frame = [3, 0, 2, 0, 0, 0, 1, 0, 9];
        check!(
            AuthFrame::parse(&frame) == Some(AuthFrame { algorithm: 3, transaction: 2, status: 0, body: &[1, 0, 9] })
        );
        check!(AuthFrame::parse(&frame[..5]).is_none());
    }
}
