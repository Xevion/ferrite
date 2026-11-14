//! nl80211 commands and the attributes the scan flow reads and writes.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::genl::{self, FamilyId, GenlMessage};
use crate::netlink::{self, Attr, DecodeError, Flags};

pub const FAMILY_NAME: &str = "nl80211";
/// The multicast group scan notifications go to.
pub const SCAN_GROUP: &str = "scan";
/// The version nl80211 requests carry, as iw sends.
const VERSION: u8 = 0;

/// The nl80211 commands this client sends or receives; anything else is kept as its number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    GetInterface,
    SetInterface,
    NewInterface,
    SetKey,
    NewKey,
    StartAp,
    StopAp,
    SetStation,
    NewStation,
    GetScan,
    TriggerScan,
    NewScanResults,
    ScanAborted,
    Authenticate,
    Associate,
    Deauthenticate,
    Disassociate,
    Connect,
    Disconnect,
    RegisterFrame,
    Frame,
    FrameTxStatus,
    SetPowerSave,
    PortAuthorized,
    ControlPortFrame,
    ControlPortFrameTxStatus,
    Unknown(u8),
}

impl Command {
    pub const fn from_raw(raw: u8) -> Self {
        match raw {
            5 => Self::GetInterface,
            6 => Self::SetInterface,
            7 => Self::NewInterface,
            10 => Self::SetKey,
            11 => Self::NewKey,
            15 => Self::StartAp,
            16 => Self::StopAp,
            18 => Self::SetStation,
            19 => Self::NewStation,
            32 => Self::GetScan,
            33 => Self::TriggerScan,
            34 => Self::NewScanResults,
            35 => Self::ScanAborted,
            37 => Self::Authenticate,
            38 => Self::Associate,
            39 => Self::Deauthenticate,
            40 => Self::Disassociate,
            46 => Self::Connect,
            48 => Self::Disconnect,
            58 => Self::RegisterFrame,
            59 => Self::Frame,
            60 => Self::FrameTxStatus,
            61 => Self::SetPowerSave,
            125 => Self::PortAuthorized,
            129 => Self::ControlPortFrame,
            139 => Self::ControlPortFrameTxStatus,
            other => Self::Unknown(other),
        }
    }

    pub const fn raw(self) -> u8 {
        match self {
            Self::GetInterface => 5,
            Self::SetInterface => 6,
            Self::NewInterface => 7,
            Self::SetKey => 10,
            Self::NewKey => 11,
            Self::StartAp => 15,
            Self::StopAp => 16,
            Self::SetStation => 18,
            Self::NewStation => 19,
            Self::GetScan => 32,
            Self::TriggerScan => 33,
            Self::NewScanResults => 34,
            Self::ScanAborted => 35,
            Self::Authenticate => 37,
            Self::Associate => 38,
            Self::Deauthenticate => 39,
            Self::Disassociate => 40,
            Self::Connect => 46,
            Self::Disconnect => 48,
            Self::RegisterFrame => 58,
            Self::Frame => 59,
            Self::FrameTxStatus => 60,
            Self::SetPowerSave => 61,
            Self::PortAuthorized => 125,
            Self::ControlPortFrame => 129,
            Self::ControlPortFrameTxStatus => 139,
            Self::Unknown(raw) => raw,
        }
    }
}

/// `enum nl80211_attrs` values used here.
pub mod attr {
    pub const WIPHY: u16 = 1;
    pub const IFINDEX: u16 = 3;
    pub const IFNAME: u16 = 4;
    pub const IFTYPE: u16 = 5;
    pub const MAC: u16 = 6;
    pub const KEY_DATA: u16 = 7;
    pub const KEY_IDX: u16 = 8;
    pub const KEY_CIPHER: u16 = 9;
    pub const KEY_SEQ: u16 = 10;
    pub const BEACON_INTERVAL: u16 = 12;
    pub const DTIM_PERIOD: u16 = 13;
    pub const BEACON_HEAD: u16 = 14;
    pub const BEACON_TAIL: u16 = 15;
    pub const STA_AID: u16 = 16;
    pub const STA_LISTEN_INTERVAL: u16 = 18;
    pub const STA_SUPPORTED_RATES: u16 = 19;
    pub const WIPHY_FREQ: u16 = 38;
    pub const WIPHY_CHANNEL_TYPE: u16 = 39;
    pub const IE: u16 = 42;
    pub const SCAN_FREQUENCIES: u16 = 44;
    pub const BSS: u16 = 47;
    pub const FRAME: u16 = 51;
    pub const SSID: u16 = 52;
    pub const AUTH_TYPE: u16 = 53;
    pub const REASON_CODE: u16 = 54;
    pub const KEY_TYPE: u16 = 55;
    pub const TIMED_OUT: u16 = 65;
    pub const STA_FLAGS2: u16 = 67;
    pub const CONTROL_PORT: u16 = 68;
    pub const PRIVACY: u16 = 70;
    pub const DISCONNECTED_BY_AP: u16 = 71;
    pub const STATUS_CODE: u16 = 72;
    pub const CIPHER_SUITES_PAIRWISE: u16 = 73;
    pub const CIPHER_SUITE_GROUP: u16 = 74;
    pub const WPA_VERSIONS: u16 = 75;
    pub const AKM_SUITES: u16 = 76;
    pub const KEY: u16 = 80;
    pub const COOKIE: u16 = 88;
    pub const FRAME_MATCH: u16 = 91;
    pub const ACK: u16 = 92;
    pub const PS_STATE: u16 = 93;
    pub const FRAME_TYPE: u16 = 101;
    pub const CONTROL_PORT_ETHERTYPE: u16 = 102;
    pub const CONTROL_PORT_NO_ENCRYPT: u16 = 103;
    pub const HIDDEN_SSID: u16 = 126;
    pub const DONT_WAIT_FOR_ACK: u16 = 142;
    pub const STA_CAPABILITY: u16 = 171;
    pub const SOCKET_OWNER: u16 = 204;
    pub const CONTROL_PORT_OVER_NL80211: u16 = 264;
    pub const CONTROL_PORT_NO_PREAUTH: u16 = 286;
}

/// `enum nl80211_bss` values used here.
mod bss_attr {
    pub const BSSID: u16 = 1;
    pub const FREQUENCY: u16 = 2;
    pub const CAPABILITY: u16 = 5;
    pub const INFORMATION_ELEMENTS: u16 = 6;
    pub const SIGNAL_MBM: u16 = 7;
    pub const BEACON_IES: u16 = 11;
}

/// A network interface index; never 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct IfIndex(u32);

impl IfIndex {
    pub const fn new(raw: u32) -> Option<Self> {
        if raw == 0 { None } else { Some(Self(raw)) }
    }

    pub const fn raw(self) -> u32 {
        self.0
    }
}

impl fmt::Display for IfIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// `enum nl80211_iftype`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IfType {
    Station,
    Ap,
    Monitor,
    P2pDevice,
    Unknown(u32),
}

impl IfType {
    pub const fn from_raw(raw: u32) -> Self {
        match raw {
            2 => Self::Station,
            3 => Self::Ap,
            6 => Self::Monitor,
            10 => Self::P2pDevice,
            other => Self::Unknown(other),
        }
    }

    pub const fn raw(self) -> u32 {
        match self {
            Self::Station => 2,
            Self::Ap => 3,
            Self::Monitor => 6,
            Self::P2pDevice => 10,
            Self::Unknown(raw) => raw,
        }
    }
}

/// A 48-bit IEEE MAC address.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MacAddr(pub [u8; 6]);

impl fmt::Display for MacAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (n, byte) in self.0.iter().enumerate() {
            if n > 0 {
                f.write_str(":")?;
            }
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// A channel's centre frequency in MHz.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Frequency(pub u32);

/// Signal strength in mBm (hundredths of a dBm).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Mbm(pub i32);

impl fmt::Display for Mbm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (whole, hundredths) = (self.0 / 100, (self.0 % 100).unsigned_abs());
        let sign = if self.0 < 0 && whole == 0 { "-" } else { "" };
        write!(f, "{sign}{whole}.{hundredths:02} dBm")
    }
}

/// A request to nl80211.
pub fn request(family: FamilyId, flags: Flags, seq: u32, cmd: Command) -> netlink::Builder {
    genl::request(family, flags, seq, cmd.raw(), VERSION)
}

/// An nl80211 message: its command and its attributes.
pub fn parse(payload: &[u8]) -> Result<(Command, GenlMessage<'_>), DecodeError> {
    let message = GenlMessage::parse(payload)?;
    Ok((Command::from_raw(message.cmd), message))
}

fn find(attrs: &[u8], kind: u16) -> Result<Option<Attr<'_>>, DecodeError> {
    for attr in netlink::attrs(attrs) {
        let attr = attr?;
        if attr.kind == kind {
            return Ok(Some(attr));
        }
    }
    Ok(None)
}

/// The interface a message is about (`NL80211_ATTR_IFINDEX`), if it names one.
pub fn ifindex(message: &GenlMessage<'_>) -> Result<Option<IfIndex>, DecodeError> {
    Ok(find(message.attrs, attr::IFINDEX)?.map(|a| a.u32()).transpose()?.and_then(IfIndex::new))
}

/// One wireless interface, from `NL80211_CMD_NEW_INTERFACE`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interface {
    pub ifindex: IfIndex,
    pub name: String,
    pub iftype: IfType,
    pub wiphy: u32,
    pub mac: MacAddr,
}

impl Interface {
    /// `None` for a wireless device with no netdev (a P2P device), which has no ifindex.
    pub fn parse(message: &GenlMessage<'_>) -> Result<Option<Self>, DecodeError> {
        let (mut ifindex, mut name, mut iftype, mut wiphy, mut mac) =
            (None, String::new(), IfType::Unknown(0), 0, MacAddr::default());
        for a in netlink::attrs(message.attrs) {
            let a = a?;
            match a.kind {
                attr::IFINDEX => ifindex = IfIndex::new(a.u32()?),
                attr::IFNAME => name = String::from(a.str()?),
                attr::IFTYPE => iftype = IfType::from_raw(a.u32()?),
                attr::WIPHY => wiphy = a.u32()?,
                attr::MAC => mac = MacAddr(a.array()?),
                _ => {},
            }
        }
        Ok(ifindex.map(|ifindex| Self { ifindex, name, iftype, wiphy, mac }))
    }
}

/// What a BSS advertises about its security.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Security {
    Open,
    /// Privacy bit set with no RSN or WPA element: WEP.
    Wep,
    Wpa,
    Rsn,
}

impl fmt::Display for Security {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Open => "open",
            Self::Wep => "WEP",
            Self::Wpa => "WPA",
            Self::Rsn => "RSN",
        })
    }
}

/// One scan result, from the `NL80211_ATTR_BSS` of `NL80211_CMD_NEW_SCAN_RESULTS`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bss {
    pub bssid: MacAddr,
    pub frequency: Frequency,
    pub signal: Option<Mbm>,
    /// Empty for a hidden network, or one whose elements carried no SSID.
    pub ssid: Vec<u8>,
    pub security: Security,
    /// The RSN element, whole, which a 4-way handshake's message 3 must repeat.
    pub rsn: Option<Vec<u8>>,
}

/// IEEE 802.11 element ids.
const ELEMENT_SSID: u8 = 0;
const ELEMENT_RSN: u8 = 48;
const ELEMENT_VENDOR: u8 = 221;
/// Microsoft's OUI and type 1: the WPA element.
const WPA_VENDOR: [u8; 4] = [0x00, 0x50, 0xf2, 0x01];
/// Capability information: privacy.
const CAPABILITY_PRIVACY: u16 = 1 << 4;

/// The `id, len, data` elements of a frame body; a truncated last element is dropped.
pub fn elements(ies: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    let mut rest = ies;
    core::iter::from_fn(move || {
        let (&id, after) = rest.split_first()?;
        let (&len, after) = after.split_first()?;
        let data = after.get(..usize::from(len))?;
        rest = &after[usize::from(len)..];
        Some((id, data))
    })
}

impl Bss {
    /// `None` for a message with no BSS attribute.
    pub fn parse(message: &GenlMessage<'_>) -> Result<Option<Self>, DecodeError> {
        let Some(bss) = find(message.attrs, attr::BSS)? else { return Ok(None) };
        let (mut bssid, mut frequency, mut signal, mut capability) = (None, None, None, 0);
        let (mut ies, mut beacon_ies) = (None, None);
        for a in bss.attrs() {
            let a = a?;
            match a.kind {
                bss_attr::BSSID => bssid = Some(MacAddr(a.array()?)),
                bss_attr::FREQUENCY => frequency = Some(Frequency(a.u32()?)),
                bss_attr::SIGNAL_MBM => signal = Some(Mbm(a.i32()?)),
                bss_attr::CAPABILITY => capability = a.u16()?,
                bss_attr::INFORMATION_ELEMENTS => ies = Some(a.payload),
                bss_attr::BEACON_IES => beacon_ies = Some(a.payload),
                _ => {},
            }
        }
        let (Some(bssid), Some(frequency)) = (bssid, frequency) else { return Ok(None) };
        let ies = ies.or(beacon_ies).unwrap_or_default();
        let ssid = elements(ies).find(|(id, _)| *id == ELEMENT_SSID).map_or_default(|(_, data)| data.to_vec());
        let rsn = elements(ies).find(|(id, _)| *id == ELEMENT_RSN).map(|(_, data)| {
            let mut element = alloc::vec![ELEMENT_RSN, data.len() as u8];
            element.extend_from_slice(data);
            element
        });
        Ok(Some(Self { bssid, frequency, signal, ssid, security: security(ies, capability), rsn }))
    }
}

/// RSN over WPA over the privacy bit, as `iw scan` reports them.
fn security(ies: &[u8], capability: u16) -> Security {
    let (mut rsn, mut wpa) = (false, false);
    for (id, data) in elements(ies) {
        match id {
            ELEMENT_RSN => rsn = true,
            ELEMENT_VENDOR if data.starts_with(&WPA_VENDOR) => wpa = true,
            _ => {},
        }
    }
    if rsn {
        Security::Rsn
    } else if wpa {
        Security::Wpa
    } else if capability & CAPABILITY_PRIVACY != 0 {
        Security::Wep
    } else {
        Security::Open
    }
}

/// An SSID for a log line: UTF-8 shown as is, other bytes escaped.
pub struct SsidDisplay<'a>(pub &'a [u8]);

impl fmt::Display for SsidDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for chunk in self.0.utf8_chunks() {
            for c in chunk.valid().chars() {
                if c.is_control() { write!(f, "\\x{:02x}", u32::from(c))? } else { write!(f, "{c}")? }
            }
            for b in chunk.invalid() {
                write!(f, "\\x{b:02x}")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;

    use assert2::check;

    use super::*;

    #[test]
    fn elements_stop_at_a_truncated_one() {
        let ies = [0, 3, b'a', b'b', b'c', 48, 2, 1, 0, 221, 9, 1];
        let found: Vec<(u8, &[u8])> = elements(&ies).collect();
        check!(found == [(0, &b"abc"[..]), (48, &[1, 0][..])]);
    }

    #[test]
    fn security_prefers_rsn_then_wpa_then_the_privacy_bit() {
        let rsn = [48, 2, 1, 0];
        let wpa = [221, 4, 0x00, 0x50, 0xf2, 0x01];
        let other_vendor = [221, 4, 0x00, 0x50, 0xf2, 0x04];
        check!(security(&[rsn.as_slice(), &wpa].concat(), CAPABILITY_PRIVACY) == Security::Rsn);
        check!(security(&wpa, CAPABILITY_PRIVACY) == Security::Wpa);
        check!(security(&other_vendor, CAPABILITY_PRIVACY) == Security::Wep);
        check!(security(&other_vendor, 0x0001) == Security::Open);
    }

    #[test]
    fn signal_prints_in_dbm() {
        check!(Mbm(-4512).to_string() == "-45.12 dBm");
        check!(Mbm(-50).to_string() == "-0.50 dBm");
    }
}
