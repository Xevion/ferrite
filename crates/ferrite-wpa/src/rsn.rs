//! The RSN element (IEEE 802.11 9.4.2.24) for WPA2-PSK with CCMP, and the KDEs in EAPOL-Key data.

use alloc::vec::Vec;

use snafu::{Snafu, ensure};

pub const ELEMENT_RSN: u8 = 48;
const ELEMENT_VENDOR: u8 = 221;
/// The IEEE 802.11 OUI suites and KDEs are named under.
const OUI: [u8; 3] = [0x00, 0x0f, 0xac];
const KDE_GTK: u8 = 1;

/// A cipher or AKM suite selector: an OUI and a type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Suite(pub [u8; 4]);

impl Suite {
    pub const CCMP: Self = Self([0x00, 0x0f, 0xac, 4]);
    pub const TKIP: Self = Self([0x00, 0x0f, 0xac, 2]);
    pub const PSK: Self = Self([0x00, 0x0f, 0xac, 2]);

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

/// What an RSN element offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rsn {
    pub group: Suite,
    pub pairwise: Vec<Suite>,
    pub akms: Vec<Suite>,
    pub capabilities: u16,
}

/// RSN capabilities: management frame protection required, and capable.
pub const CAP_MFPR: u16 = 1 << 6;

impl Rsn {
    /// WPA2-PSK with CCMP for both ciphers, no capabilities: what this station offers.
    pub fn psk_ccmp() -> Self {
        Self { group: Suite::CCMP, pairwise: alloc::vec![Suite::CCMP], akms: alloc::vec![Suite::PSK], capabilities: 0 }
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
        let mut rsn = Self { group: Suite::CCMP, pairwise: Vec::new(), akms: Vec::new(), capabilities: 0 };
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
        if let Some(caps) = take(2) {
            rsn.capabilities = u16::from_le_bytes([caps[0], caps[1]]);
        }
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
}

/// The first RSN element in a run of elements, whole.
pub fn find_element(ies: &[u8]) -> Option<&[u8]> {
    let mut rest = ies;
    while let [id, len, after @ ..] = rest {
        let len = usize::from(*len);
        if after.len() < len {
            return None;
        }
        if *id == ELEMENT_RSN {
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

/// What decrypted key data carries that the station uses.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyData<'a> {
    pub rsn: Option<&'a [u8]>,
    pub gtk: Option<Gtk>,
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
            ELEMENT_VENDOR if body.len() >= 6 && body[..3] == OUI && body[3] == KDE_GTK => {
                found.gtk = Some(Gtk { index: body[4] & 3, key: body[6..].to_vec() });
            },
            _ => {},
        }
        rest = &after[len..];
    }
    found
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
}
