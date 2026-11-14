//! The RSN element (IEEE 802.11 9.4.2.24) for WPA2-PSK and WPA3-SAE with CCMP, the RSNX element that
//! says a station does hash-to-element, and the KDEs in EAPOL-Key data.

use alloc::vec::Vec;

use snafu::{Snafu, ensure};

use crate::eapol::DescriptorVersion;

pub const ELEMENT_RSN: u8 = 48;
/// The RSN extension element: capabilities that do not fit the RSN element's fields.
pub const ELEMENT_RSNX: u8 = 244;
const ELEMENT_VENDOR: u8 = 221;
/// The IEEE 802.11 OUI suites and KDEs are named under.
const OUI: [u8; 3] = [0x00, 0x0f, 0xac];
const KDE_GTK: u8 = 1;
const KDE_IGTK: u8 = 9;
/// RSNX capability bit 5 of the first octet: SAE hash-to-element.
const RSNX_H2E: u8 = 0x20;
/// The RSNX element a station that does hash-to-element puts in its association request.
pub const RSNX_H2E_ELEMENT: [u8; 3] = [ELEMENT_RSNX, 1, RSNX_H2E];
pub const PMKID_LEN: usize = 16;

/// A cipher or AKM suite selector: an OUI and a type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Suite(pub [u8; 4]);

impl Suite {
    pub const CCMP: Self = Self([0x00, 0x0f, 0xac, 4]);
    pub const TKIP: Self = Self([0x00, 0x0f, 0xac, 2]);
    pub const PSK: Self = Self([0x00, 0x0f, 0xac, 2]);
    /// WPA3-Personal (SAE with SHA-256).
    pub const SAE: Self = Self([0x00, 0x0f, 0xac, 8]);
    /// The group management cipher: BIP with AES-128-CMAC, which protected management frames use.
    pub const BIP_CMAC_128: Self = Self([0x00, 0x0f, 0xac, 6]);

    /// The selector as nl80211 carries it: a u32 of the bytes read big-endian.
    pub const fn nl80211(self) -> u32 {
        u32::from_be_bytes(self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Snafu)]
pub enum RsnError {
    #[snafu(display("the RSN element is truncated at byte {at}"))]
    Truncated { at: usize },
    #[snafu(display("RSN version {version} is not 1"))]
    Version { version: u16 },
}

/// The key management a passphrase network is joined with: what the access point's RSN element
/// offers and the station selects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Akm {
    /// WPA2-PSK: the PMK from the passphrase, EAPOL-Key version 2 (HMAC-SHA1).
    Psk,
    /// WPA3-SAE: the PMK from the SAE exchange, key descriptor version 0 (AKM-defined: AES-128-CMAC),
    /// the SHA-256 KDF, and protected management frames required.
    Sae,
}

impl Akm {
    pub const fn suite(self) -> Suite {
        match self {
            Self::Psk => Suite::PSK,
            Self::Sae => Suite::SAE,
        }
    }

    /// The Key Descriptor Version both sides' EAPOL-Key frames carry.
    pub const fn descriptor_version(self) -> DescriptorVersion {
        match self {
            Self::Psk => DescriptorVersion::Sha1Aes,
            Self::Sae => DescriptorVersion::AkmDefined,
        }
    }

    /// Whether management frame protection is required, and so an IGTK arrives in message 3.
    pub const fn requires_pmf(self) -> bool {
        matches!(self, Self::Sae)
    }
}

/// What an RSN element offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rsn {
    pub group: Suite,
    pub pairwise: Vec<Suite>,
    pub akms: Vec<Suite>,
    pub capabilities: u16,
    /// PMKIDs a (re)association request names.
    pub pmkids: Vec<[u8; PMKID_LEN]>,
    /// The group management cipher; `None` when the element ends before it (BIP-CMAC-128 then).
    pub group_mgmt: Option<Suite>,
}

/// RSN capabilities: management frame protection required, and capable.
pub const CAP_MFPR: u16 = 1 << 6;
pub const CAP_MFPC: u16 = 1 << 7;

impl Rsn {
    /// WPA2-PSK with CCMP for both ciphers, no capabilities: what this station offers.
    pub fn psk_ccmp() -> Self {
        Self {
            group: Suite::CCMP,
            pairwise: alloc::vec![Suite::CCMP],
            akms: alloc::vec![Suite::PSK],
            capabilities: 0,
            pmkids: Vec::new(),
            group_mgmt: None,
        }
    }

    /// WPA3-Personal with CCMP and protected management frames required and offered, as a station
    /// sends it; `pmkid` is the one SAE derived, named in the association request.
    pub fn sae_ccmp(pmkid: Option<[u8; PMKID_LEN]>) -> Self {
        Self {
            group: Suite::CCMP,
            pairwise: alloc::vec![Suite::CCMP],
            akms: alloc::vec![Suite::SAE],
            capabilities: CAP_MFPR | CAP_MFPC,
            pmkids: pmkid.into_iter().collect(),
            group_mgmt: Some(Suite::BIP_CMAC_128),
        }
    }

    /// Parse the body of an RSN element (after id and length). Missing trailing fields take the
    /// standard defaults (CCMP and 802.1X), which a PSK network never relies on.
    pub fn parse(body: &[u8]) -> Result<Self, RsnError> {
        let mut at = 0;
        let mut take = |n: usize| -> Option<&[u8]> {
            let bytes = body.get(at..at + n)?;
            at += n;
            Some(bytes)
        };
        let version = take(2).map(|b| u16::from_le_bytes([b[0], b[1]])).ok_or(RsnError::Truncated { at: 0 })?;
        ensure!(version == 1, VersionSnafu { version });
        let mut rsn = Self {
            group: Suite::CCMP,
            pairwise: Vec::new(),
            akms: Vec::new(),
            capabilities: 0,
            pmkids: Vec::new(),
            group_mgmt: None,
        };
        let Some(group) = take(4) else { return Ok(rsn) };
        rsn.group = Suite(group.try_into().unwrap_or_default());
        for n in 0..2 {
            let Some(count) = take(2) else { return Ok(rsn) };
            let count = usize::from(u16::from_le_bytes([count[0], count[1]]));
            for _ in 0..count {
                let suite = take(4).ok_or(RsnError::Truncated { at: body.len() })?;
                let suite = Suite(suite.try_into().unwrap_or_default());
                if n == 0 { rsn.pairwise.push(suite) } else { rsn.akms.push(suite) }
            }
        }
        let Some(caps) = take(2) else { return Ok(rsn) };
        rsn.capabilities = u16::from_le_bytes([caps[0], caps[1]]);
        let Some(count) = take(2) else { return Ok(rsn) };
        for _ in 0..u16::from_le_bytes([count[0], count[1]]) {
            let pmkid = take(PMKID_LEN).ok_or(RsnError::Truncated { at: body.len() })?;
            rsn.pmkids.push(pmkid.try_into().unwrap_or_default());
        }
        rsn.group_mgmt = take(4).map(|suite| Suite(suite.try_into().unwrap_or_default()));
        Ok(rsn)
    }

    /// The whole element, id and length included.
    pub fn element(&self) -> Vec<u8> {
        let mut body = Vec::with_capacity(20);
        body.extend_from_slice(&1_u16.to_le_bytes());
        body.extend_from_slice(&self.group.0);
        for list in [&self.pairwise, &self.akms] {
            body.extend_from_slice(&(list.len() as u16).to_le_bytes());
            for suite in list {
                body.extend_from_slice(&suite.0);
            }
        }
        body.extend_from_slice(&self.capabilities.to_le_bytes());
        if !self.pmkids.is_empty() || self.group_mgmt.is_some() {
            body.extend_from_slice(&(self.pmkids.len() as u16).to_le_bytes());
            for pmkid in &self.pmkids {
                body.extend_from_slice(pmkid);
            }
        }
        if let Some(suite) = self.group_mgmt {
            body.extend_from_slice(&suite.0);
        }
        let mut element = alloc::vec![ELEMENT_RSN, body.len() as u8];
        element.extend_from_slice(&body);
        element
    }

    /// Whether a station offering only [`Rsn::psk_ccmp`] can join: PSK, CCMP pairwise and group, and
    /// management frame protection not required.
    pub fn supports_psk_ccmp(&self) -> bool {
        self.group == Suite::CCMP
            && self.pairwise.contains(&Suite::CCMP)
            && self.akms.contains(&Suite::PSK)
            && self.capabilities & CAP_MFPR == 0
    }

    /// Whether a station that offers [`Rsn::sae_ccmp`] can join: SAE with CCMP for both ciphers,
    /// the access point capable of protected management frames, and BIP-CMAC-128 for them.
    pub fn supports_sae_ccmp(&self) -> bool {
        self.group == Suite::CCMP
            && self.pairwise.contains(&Suite::CCMP)
            && self.akms.contains(&Suite::SAE)
            && self.capabilities & CAP_MFPC != 0
            && self.group_mgmt.is_none_or(|suite| suite == Suite::BIP_CMAC_128)
    }
}

/// Whether an RSNX element (whole) says the sender does SAE hash-to-element.
pub fn rsnx_has_h2e(element: &[u8]) -> bool {
    matches!(element, [ELEMENT_RSNX, len, first, ..] if *len >= 1 && first & RSNX_H2E != 0)
}

/// The first RSN element in a run of elements, whole.
pub fn find_element(ies: &[u8]) -> Option<&[u8]> {
    find_id(ies, ELEMENT_RSN)
}

/// The first RSNX element in a run of elements, whole.
pub fn find_rsnx(ies: &[u8]) -> Option<&[u8]> {
    find_id(ies, ELEMENT_RSNX)
}

fn find_id(ies: &[u8], wanted: u8) -> Option<&[u8]> {
    let mut rest = ies;
    while let [id, len, after @ ..] = rest {
        let len = usize::from(*len);
        if after.len() < len {
            return None;
        }
        if *id == wanted {
            return Some(&rest[..2 + len]);
        }
        rest = &after[len..];
    }
    None
}

/// A group key from a GTK KDE.
#[derive(Clone, PartialEq, Eq)]
pub struct Gtk {
    /// 1 to 3 in practice; 0 is legal but unusual.
    pub index: u8,
    pub key: Vec<u8>,
}

impl core::fmt::Debug for Gtk {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Gtk {{ index: {}, {} bytes }}", self.index, self.key.len())
    }
}

/// An integrity group key (protected management frames), from an IGTK KDE.
#[derive(Clone, PartialEq, Eq)]
pub struct Igtk {
    /// 4 or 5.
    pub index: u16,
    /// The packet number the key starts at, little-endian.
    pub ipn: [u8; 6],
    pub key: [u8; 16],
}

impl core::fmt::Debug for Igtk {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Igtk {{ index: {}, 16 bytes }}", self.index)
    }
}

/// What decrypted key data carries that the station uses.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyData<'a> {
    pub rsn: Option<&'a [u8]>,
    pub rsnx: Option<&'a [u8]>,
    pub gtk: Option<Gtk>,
    pub igtk: Option<Igtk>,
}

/// Walk key data: elements and KDEs, stopping at the 0xdd padding.
pub fn parse_key_data(data: &[u8]) -> KeyData<'_> {
    let mut found = KeyData::default();
    let mut rest = data;
    while let [id, len, after @ ..] = rest {
        if *id == ELEMENT_VENDOR && *len == 0 {
            break;
        }
        let len = usize::from(*len);
        let Some(body) = after.get(..len) else { break };
        match *id {
            ELEMENT_RSN if found.rsn.is_none() => found.rsn = Some(&rest[..2 + len]),
            ELEMENT_RSNX if found.rsnx.is_none() => found.rsnx = Some(&rest[..2 + len]),
            ELEMENT_VENDOR if body.len() >= 6 && body[..3] == OUI && body[3] == KDE_GTK => {
                found.gtk = Some(Gtk { index: body[4] & 3, key: body[6..].to_vec() });
            },
            ELEMENT_VENDOR if body.len() == 4 + 2 + 6 + 16 && body[..3] == OUI && body[3] == KDE_IGTK => {
                let index = u16::from_le_bytes([body[4], body[5]]);
                if (4..=5).contains(&index) {
                    found.igtk = Some(Igtk {
                        index,
                        ipn: body[6..12].try_into().unwrap_or_default(),
                        key: body[12..].try_into().unwrap_or_default(),
                    });
                }
            },
            _ => {},
        }
        rest = &after[len..];
    }
    found
}

/// An IGTK KDE.
pub fn igtk_kde(igtk: &Igtk) -> Vec<u8> {
    let mut kde = alloc::vec![ELEMENT_VENDOR, 4 + 2 + 6 + 16];
    kde.extend_from_slice(&OUI);
    kde.push(KDE_IGTK);
    kde.extend_from_slice(&igtk.index.to_le_bytes());
    kde.extend_from_slice(&igtk.ipn);
    kde.extend_from_slice(&igtk.key);
    kde
}

/// A GTK KDE with the Tx bit clear.
pub fn gtk_kde(gtk: &Gtk) -> Vec<u8> {
    let mut kde = alloc::vec![ELEMENT_VENDOR, (6 + gtk.key.len()) as u8];
    kde.extend_from_slice(&OUI);
    kde.extend_from_slice(&[KDE_GTK, gtk.index & 3, 0]);
    kde.extend_from_slice(&gtk.key);
    kde
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};
    use rstest::rstest;

    use super::*;

    #[test]
    fn the_psk_ccmp_element_is_the_twenty_bytes_wpa_supplicant_sends() {
        check!(
            Rsn::psk_ccmp().element()
                == [
                    0x30, 0x14, 0x01, 0x00, 0x00, 0x0f, 0xac, 0x04, 0x01, 0x00, 0x00, 0x0f, 0xac, 0x04, 0x01, 0x00,
                    0x00, 0x0f, 0xac, 0x02, 0x00, 0x00
                ]
        );
    }

    #[test]
    fn a_mixed_mode_element_offers_psk_ccmp_unless_mfp_is_required() {
        // Group TKIP would rule it out; pairwise TKIP+CCMP and AKMs PSK+SAE(8) do not.
        let mut body = alloc::vec![1, 0, 0x00, 0x0f, 0xac, 4, 2, 0, 0x00, 0x0f, 0xac, 2, 0x00, 0x0f, 0xac, 4];
        body.extend_from_slice(&[2, 0, 0x00, 0x0f, 0xac, 2, 0x00, 0x0f, 0xac, 8, 0x80, 0]);
        assert!(let Ok(rsn) = Rsn::parse(&body));
        check!(rsn.pairwise == [Suite::TKIP, Suite::CCMP]);
        check!(rsn.supports_psk_ccmp());
        let len = body.len();
        body[len - 2] |= 0x40;
        check!(!Rsn::parse(&body).unwrap().supports_psk_ccmp());
        check!(Rsn::parse(&[1, 0, 0x00, 0x0f, 0xac, 4, 1, 0, 0x00]) == Err(RsnError::Truncated { at: 9 }));
    }

    #[test]
    fn key_data_yields_the_rsn_element_and_gtk_and_stops_at_padding() {
        let gtk = Gtk { index: 2, key: alloc::vec![9; 16] };
        let mut data = Rsn::psk_ccmp().element();
        data.extend_from_slice(&gtk_kde(&gtk));
        data.extend_from_slice(&[0xdd, 0, 0x30, 1]);
        let found = parse_key_data(&data);
        check!(found.rsn == Some(&Rsn::psk_ccmp().element()[..]));
        check!(found.gtk == Some(gtk));
    }

    #[rstest]
    #[case::psk(Rsn::psk_ccmp())]
    #[case::sae(Rsn::sae_ccmp(None))]
    #[case::sae_with_pmkid(Rsn::sae_ccmp(Some([7; PMKID_LEN])))]
    fn an_element_parses_back_to_what_wrote_it(#[case] rsn: Rsn) {
        let element = rsn.element();
        check!(element[0] == ELEMENT_RSN);
        check!(usize::from(element[1]) == element.len() - 2);
        check!(Rsn::parse(&element[2..]) == Ok(rsn));
    }

    #[test]
    fn the_sae_station_element_is_the_wpa3_personal_one() {
        check!(
            Rsn::sae_ccmp(None).element()
                == [
                    0x30, 0x1a, 0x01, 0x00, 0x00, 0x0f, 0xac, 0x04, 0x01, 0x00, 0x00, 0x0f, 0xac, 0x04, 0x01, 0x00,
                    0x00, 0x0f, 0xac, 0x08, 0xc0, 0x00, 0x00, 0x00, 0x00, 0x0f, 0xac, 0x06
                ]
        );
    }

    fn offered(akms: &[Suite], capabilities: u16, group_mgmt: Option<Suite>) -> Rsn {
        Rsn { akms: akms.to_vec(), capabilities, group_mgmt, ..Rsn::psk_ccmp() }
    }

    #[rstest]
    #[case::wpa2_only(offered(&[Suite::PSK], 0, None), true, false)]
    #[case::wpa3_only(offered(&[Suite::SAE], CAP_MFPR | CAP_MFPC, Some(Suite::BIP_CMAC_128)), false, true)]
    #[case::transition(offered(&[Suite::PSK, Suite::SAE], CAP_MFPC, Some(Suite::BIP_CMAC_128)), true, true)]
    #[case::sae_without_pmf(offered(&[Suite::SAE], 0, None), false, false)]
    #[case::sae_with_another_bip(offered(&[Suite::SAE], CAP_MFPC, Some(Suite([0, 0x0f, 0xac, 12]))), false, false)]
    fn the_offered_akms_decide_what_a_station_can_join(#[case] rsn: Rsn, #[case] psk: bool, #[case] sae: bool) {
        check!(rsn.supports_psk_ccmp() == psk);
        check!(rsn.supports_sae_ccmp() == sae);
    }

    #[rstest]
    #[case::h2e(&[244, 1, 0x20], true)]
    #[case::other_capability(&[244, 1, 0x10], false)]
    #[case::empty(&[244, 0], false)]
    #[case::not_rsnx(&[48, 1, 0x20], false)]
    fn the_rsnx_element_says_whether_hash_to_element_is_offered(#[case] element: &[u8], #[case] want: bool) {
        check!(rsnx_has_h2e(element) == want);
        check!(RSNX_H2E_ELEMENT == [244, 1, 0x20]);
    }

    #[test]
    fn key_data_yields_rsnx_and_igtk() {
        let igtk = Igtk { index: 5, ipn: [1, 2, 3, 4, 5, 6], key: [9; 16] };
        let mut data = Rsn::sae_ccmp(None).element();
        data.extend_from_slice(&RSNX_H2E_ELEMENT);
        data.extend_from_slice(&gtk_kde(&Gtk { index: 1, key: alloc::vec![3; 16] }));
        data.extend_from_slice(&igtk_kde(&igtk));
        let found = parse_key_data(&data);
        check!(found.rsnx == Some(&RSNX_H2E_ELEMENT[..]));
        check!(found.igtk == Some(igtk));
        check!(find_rsnx(&data) == Some(&RSNX_H2E_ELEMENT[..]));
    }

    #[rstest]
    #[case::below(3)]
    #[case::above(6)]
    fn an_igtk_with_an_index_outside_4_and_5_is_ignored(#[case] index: u16) {
        let kde = igtk_kde(&Igtk { index, ipn: [0; 6], key: [1; 16] });
        check!(parse_key_data(&kde).igtk.is_none());
    }
}
