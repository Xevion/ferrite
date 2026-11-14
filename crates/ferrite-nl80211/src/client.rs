//! An nl80211 client over any netlink socket, doing what `iw` does for a scan and what iwd does for a
//! connection.
//!
//! Two sockets: requests and their replies go over one, and the other joins the "scan" multicast group,
//! so a dump's replies and scan notifications never share a queue. A request without a dump asks for an
//! ack and ends at it; a dump ends at `NLMSG_DONE`. The request socket is also the connection owner:
//! EAPOL frames over the control port, registered management frames and their transmit status are
//! unicast to it, and it joins the "mlme" group for connect and disconnect events. Those arrive
//! interleaved with replies, so an exchange keeps every message of another sequence number, in order,
//! for [`Client::next_event`].

use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;
use core::time::Duration;

use ferrite_hal::Instant;
use snafu::{OptionExt, ResultExt, Snafu};

use crate::genl::{self, Family, GenlError, GenlMessage, GroupId};
use crate::mlme::{self, Event, ReasonCode};
use crate::netlink::{self, DecodeError, Errno, ErrorMessage, Flags, MessageType};
use crate::nl80211::{self, Bss, Command, Frequency, IfIndex, IfType, Interface, MacAddr, attr};

/// Room for the largest datagram: the kernel sizes dump skbs from the receive buffer, at most 32 KiB.
const RECV_BUFFER: usize = 64 << 10;
/// How long one reply may take.
const REPLY_TIMEOUT: Duration = Duration::from_secs(2);

/// A netlink socket of `NETLINK_GENERIC`, sending to the kernel.
pub trait Socket {
    fn send(&mut self, message: &[u8]) -> Result<(), Errno>;
    /// One datagram into `buf`: its length, or `None` once `timeout` passes with nothing to read.
    fn recv(&mut self, buf: &mut [u8], timeout: Duration) -> Result<Option<usize>, Errno>;
    fn join(&mut self, group: GroupId) -> Result<(), Errno>;
}

#[derive(Debug, Snafu)]
pub enum ClientError {
    #[snafu(display("sending {what} failed: {errno}"))]
    Send { what: &'static str, errno: Errno },
    #[snafu(display("receiving the reply to {what} failed: {errno}"))]
    Recv { what: &'static str, errno: Errno },
    #[snafu(display("no reply to {what} within {} ms", timeout.as_millis()))]
    Timeout { what: &'static str, timeout: Duration },
    #[snafu(display("{what} failed: {errno}{}", text.as_deref().map_or_default(|t| alloc::format!(" ({t})"))))]
    Kernel { what: &'static str, errno: Errno, text: Option<alloc::string::String> },
    #[snafu(display("the reply to {what} is malformed: {source}"))]
    Decode { what: &'static str, source: DecodeError },
    #[snafu(display("the {name} family: {source}"))]
    Family { name: &'static str, source: GenlError },
    #[snafu(display("the controller sent no {name} family"))]
    NoFamily { name: &'static str },
    #[snafu(display("the {family} family has no {group:?} multicast group"))]
    NoGroup { family: &'static str, group: &'static str },
    #[snafu(display("joining the {group:?} group failed: {errno}"))]
    Join { group: &'static str, errno: Errno },
}

/// The payload of a message received in reply, copied out of its datagram.
#[derive(Clone, Debug)]
struct Reply {
    payload: Vec<u8>,
}

/// How a scan ended, as the scan group announced it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanEnd {
    Done,
    Aborted,
}

/// The cipher and AKM suite selectors nl80211 takes, and the WPA version bit.
pub const CIPHER_CCMP: u32 = 0x000f_ac04;
/// BIP-CMAC-128, the cipher of the integrity group key.
pub const CIPHER_AES_CMAC: u32 = 0x000f_ac06;
pub const AKM_PSK: u32 = 0x000f_ac02;
pub const AKM_SAE: u32 = 0x000f_ac08;
const WPA_VERSION_2: u32 = 2;
/// `NL80211_AUTHTYPE_SAE`, `NL80211_MFP_REQUIRED` and `NL80211_HIDDEN_SSID_ZERO_LEN`.
const AUTHTYPE_SAE: u32 = 4;
const MFP_REQUIRED: u32 = 1;
const HIDDEN_SSID_ZERO_LEN: u32 = 1;
/// `NL80211_STA_FLAG_*` bits.
const STA_FLAG_AUTHORIZED: u32 = 1 << 1;
const STA_FLAG_MFP: u32 = 1 << 4;
const STA_FLAG_AUTHENTICATED: u32 = 1 << 5;
const STA_FLAG_ASSOCIATED: u32 = 1 << 7;
/// `enum nl80211_key_attributes` and `nl80211_key_default_types`.
mod key_attr {
    pub const DATA: u16 = 1;
    pub const IDX: u16 = 2;
    pub const CIPHER: u16 = 3;
    pub const SEQ: u16 = 4;
    pub const DEFAULT: u16 = 5;
    pub const DEFAULT_MGMT: u16 = 6;
    pub const TYPE: u16 = 7;
    pub const DEFAULT_TYPES: u16 = 8;
    pub const DEFAULT_TYPE_MULTICAST: u16 = 2;
}
const KEYTYPE_GROUP: u32 = 0;
const KEYTYPE_PAIRWISE: u32 = 1;

/// A network to start on an AP interface: open, or WPA2-PSK and WPA3-SAE with CCMP when `rsn` holds the
/// RSN element (the beacon's tail, with any RSNX element after it) and `akms` the suites it offers.
pub struct ApConfig<'a> {
    pub ssid: &'a [u8],
    pub rsn: Option<&'a [u8]>,
    /// The AKM suite selectors of `rsn`, as `AKM_*`.
    pub akms: &'a [u32],
    /// The beacon carries a zero-length SSID; stations ask by name.
    pub hidden: bool,
    pub frequency: Frequency,
    /// In TU (1024 us); cfg80211 takes 10 to 10000.
    pub beacon_interval: u16,
    /// The 802.11 channel number the DS Parameter Set element names.
    pub channel: u8,
}

/// A WPA2-PSK connection, as iwd asks for one: the supplicant runs in userspace and EAPOL goes over
/// nl80211 to this socket. Without an RSN element the connection is to an open network.
pub struct ConnectParams<'a> {
    pub ssid: &'a [u8],
    pub bssid: Option<MacAddr>,
    pub frequency: Option<Frequency>,
    /// The RSN element for the association request; `None` for an open network.
    pub rsn: Option<&'a [u8]>,
}

/// The association after an SAE authentication, which cfg80211's SME cannot run for mac80211: the
/// authentication frames went through [`Client::authenticate_sae`].
pub struct AssociateParams<'a> {
    pub ssid: &'a [u8],
    pub bssid: MacAddr,
    pub frequency: Frequency,
    /// The elements for the association request: the RSN element (with the PMKID) and the RSNX element.
    pub ies: &'a [u8],
}

/// A key for `NL80211_CMD_NEW_KEY`.
pub struct KeyParams<'a> {
    /// The peer for a pairwise key; `None` for a group key.
    pub peer: Option<MacAddr>,
    pub index: u8,
    pub cipher: u32,
    pub data: &'a [u8],
    /// The receive sequence counter to start from, 6 bytes for CCMP.
    pub seq: Option<&'a [u8]>,
}

/// A station an AP admits after association (`NL80211_CMD_NEW_STATION`).
pub struct StationParams<'a> {
    pub mac: MacAddr,
    pub aid: u16,
    pub listen_interval: u16,
    pub capability: u16,
    pub supported_rates: &'a [u8],
    /// The station negotiated protected management frames.
    pub mfp: bool,
}

pub struct Client<S: Socket> {
    requests: S,
    events: S,
    family: Family,
    seq: u32,
    buf: Vec<u8>,
    /// Messages for the request socket that were not replies, oldest first.
    pending: VecDeque<Vec<u8>>,
}

impl<S: Socket> Client<S> {
    /// Resolve nl80211 through the controller and join its scan group on `events`.
    pub fn connect(mut requests: S, mut events: S) -> Result<Self, ClientError> {
        let mut buf = vec![0; RECV_BUFFER];
        let (replies, _) =
            exchange(&mut requests, &mut buf, &genl::get_family(1, nl80211::FAMILY_NAME), 1, false, "GETFAMILY")?;
        let reply = replies.first().context(NoFamilySnafu { name: nl80211::FAMILY_NAME })?;
        let message = GenlMessage::parse(&reply.payload).context(DecodeSnafu { what: "GETFAMILY" })?;
        let family = Family::parse(&message).context(FamilySnafu { name: nl80211::FAMILY_NAME })?;
        let group = family
            .group(nl80211::SCAN_GROUP)
            .context(NoGroupSnafu { family: nl80211::FAMILY_NAME, group: nl80211::SCAN_GROUP })?;
        events.join(group).map_err(|errno| ClientError::Join { group: nl80211::SCAN_GROUP, errno })?;
        Ok(Self { requests, events, family, seq: 1, buf, pending: VecDeque::new() })
    }

    pub const fn family(&self) -> &Family {
        &self.family
    }

    fn next_seq(&mut self) -> u32 {
        self.seq = self.seq.wrapping_add(1).max(1);
        self.seq
    }

    fn run(
        &mut self,
        cmd: Command,
        dump: bool,
        what: &'static str,
        attrs: impl FnOnce(netlink::Builder) -> netlink::Builder,
    ) -> Result<Vec<Reply>, ClientError> {
        let seq = self.next_seq();
        let flags = if dump { Flags::DUMP } else { Flags::ACK };
        let message = attrs(nl80211::request(self.family.id, flags, seq, cmd)).finish();
        let (replies, stashed) = exchange(&mut self.requests, &mut self.buf, &message, seq, dump, what)?;
        self.pending.extend(stashed.into_iter().map(|reply| reply.payload));
        Ok(replies)
    }

    /// Join the "mlme" group on the request socket, so connect and disconnect events arrive in order
    /// with the frames the connection owner receives.
    pub fn join_mlme(&mut self) -> Result<(), ClientError> {
        let group = self
            .family
            .group(mlme::MLME_GROUP)
            .context(NoGroupSnafu { family: nl80211::FAMILY_NAME, group: mlme::MLME_GROUP })?;
        self.requests.join(group).map_err(|errno| ClientError::Join { group: mlme::MLME_GROUP, errno })
    }

    /// The next notification for the request socket, until `deadline` on `clock`.
    pub fn next_event(&mut self, deadline: Instant, clock: &dyn Fn() -> Instant) -> Result<Option<Event>, ClientError> {
        const WHAT: &str = "an nl80211 event";
        loop {
            if let Some(payload) = self.pending.pop_front() {
                let (cmd, message) = nl80211::parse(&payload).context(DecodeSnafu { what: WHAT })?;
                return Event::parse(cmd, &message).map(Some).context(DecodeSnafu { what: WHAT });
            }
            let left = deadline.saturating_duration_since(clock());
            if left.is_zero() {
                return Ok(None);
            }
            let Some(len) =
                self.requests.recv(&mut self.buf, left).map_err(|errno| ClientError::Recv { what: WHAT, errno })?
            else {
                return Ok(None);
            };
            for message in netlink::messages(&self.buf[..len]) {
                let message = message.context(DecodeSnafu { what: WHAT })?;
                if message.kind == self.family.id.message_type() {
                    self.pending.push_back(message.payload.to_vec());
                }
            }
        }
    }

    /// Ask cfg80211's SME to authenticate and associate (`NL80211_CMD_CONNECT`), owning the connection.
    /// With an RSN element it is WPA2-PSK with CCMP and takes EAPOL over nl80211; without, an open
    /// network with no keys and no handshake.
    pub fn connect_network(&mut self, ifindex: IfIndex, params: &ConnectParams<'_>) -> Result<(), ClientError> {
        self.run(Command::Connect, false, "CONNECT", |b| {
            let mut b = b.u32(attr::IFINDEX, ifindex.raw()).attr(attr::SSID, params.ssid);
            if let Some(bssid) = params.bssid {
                b = b.attr(attr::MAC, &bssid.0);
            }
            if let Some(frequency) = params.frequency {
                b = b.u32(attr::WIPHY_FREQ, frequency.0);
            }
            let b = b.u32(attr::AUTH_TYPE, 0).flag(attr::SOCKET_OWNER);
            let Some(rsn) = params.rsn else { return b };
            b.attr(attr::IE, rsn)
                .flag(attr::PRIVACY)
                .u32(attr::WPA_VERSIONS, WPA_VERSION_2)
                .u32(attr::AKM_SUITES, AKM_PSK)
                .u32(attr::CIPHER_SUITES_PAIRWISE, CIPHER_CCMP)
                .u32(attr::CIPHER_SUITE_GROUP, CIPHER_CCMP)
                .flag(attr::CONTROL_PORT)
                .flag(attr::CONTROL_PORT_OVER_NL80211)
                .flag(attr::CONTROL_PORT_NO_PREAUTH)
        })
        .map(drop)
    }

    /// Send one SAE authentication frame (`NL80211_CMD_AUTHENTICATE`): `auth_data` is the transaction
    /// number and status code (16-bit little-endian each) and the frame body. The access point's
    /// answers arrive as [`Event::Authenticate`](crate::mlme::Event::Authenticate).
    pub fn authenticate_sae(
        &mut self,
        ifindex: IfIndex,
        ssid: &[u8],
        bssid: MacAddr,
        frequency: Frequency,
        auth_data: &[u8],
    ) -> Result<(), ClientError> {
        self.run(Command::Authenticate, false, "AUTHENTICATE", |b| {
            b.u32(attr::IFINDEX, ifindex.raw())
                .attr(attr::MAC, &bssid.0)
                .attr(attr::SSID, ssid)
                .u32(attr::WIPHY_FREQ, frequency.0)
                .u32(attr::AUTH_TYPE, AUTHTYPE_SAE)
                .attr(attr::AUTH_DATA, auth_data)
        })
        .map(drop)
    }

    /// Associate after an SAE authentication (`NL80211_CMD_ASSOCIATE`) with protected management
    /// frames required, owning the connection and taking EAPOL over nl80211. The result arrives as
    /// `Event::Connect`, as for [`Client::connect_network`].
    pub fn associate_sae(&mut self, ifindex: IfIndex, params: &AssociateParams<'_>) -> Result<(), ClientError> {
        self.run(Command::Associate, false, "ASSOCIATE", |b| {
            b.u32(attr::IFINDEX, ifindex.raw())
                .attr(attr::MAC, &params.bssid.0)
                .attr(attr::SSID, params.ssid)
                .u32(attr::WIPHY_FREQ, params.frequency.0)
                .attr(attr::IE, params.ies)
                .u32(attr::USE_MFP, MFP_REQUIRED)
                .u32(attr::WPA_VERSIONS, WPA_VERSION_2)
                .u32(attr::AKM_SUITES, AKM_SAE)
                .u32(attr::CIPHER_SUITES_PAIRWISE, CIPHER_CCMP)
                .u32(attr::CIPHER_SUITE_GROUP, CIPHER_CCMP)
                .flag(attr::CONTROL_PORT)
                .flag(attr::CONTROL_PORT_OVER_NL80211)
                .flag(attr::CONTROL_PORT_NO_PREAUTH)
                .flag(attr::SOCKET_OWNER)
        })
        .map(drop)
    }

    /// Leave the network (`NL80211_CMD_DISCONNECT`).
    pub fn disconnect(&mut self, ifindex: IfIndex, reason: ReasonCode) -> Result<(), ClientError> {
        self.run(Command::Disconnect, false, "DISCONNECT", |b| {
            b.u32(attr::IFINDEX, ifindex.raw()).u16(attr::REASON_CODE, reason.raw())
        })
        .map(drop)
    }

    /// Turn 802.11 power save on or off (`NL80211_CMD_SET_POWER_SAVE`). Off keeps the radio awake, so
    /// frames to the station arrive at once instead of at the access point's next beacon.
    pub fn set_power_save(&mut self, ifindex: IfIndex, enabled: bool) -> Result<(), ClientError> {
        self.run(Command::SetPowerSave, false, "SET_POWER_SAVE", |b| {
            b.u32(attr::IFINDEX, ifindex.raw()).u32(attr::PS_STATE, u32::from(enabled))
        })
        .map(drop)
    }

    /// Send an EAPOL frame (802.1X header on) to `dest` over the control port.
    pub fn send_eapol(&mut self, ifindex: IfIndex, dest: MacAddr, frame: &[u8]) -> Result<(), ClientError> {
        self.run(Command::ControlPortFrame, false, "CONTROL_PORT_FRAME", |b| {
            b.u32(attr::IFINDEX, ifindex.raw())
                .attr(attr::FRAME, frame)
                .attr(attr::MAC, &dest.0)
                .u16(attr::CONTROL_PORT_ETHERTYPE, ETH_P_PAE)
                .flag(attr::DONT_WAIT_FOR_ACK)
        })
        .map(drop)
    }

    /// Install a key (`NL80211_CMD_NEW_KEY`).
    pub fn new_key(&mut self, ifindex: IfIndex, key: &KeyParams<'_>) -> Result<(), ClientError> {
        let what = if key.peer.is_some() { "NEW_KEY (pairwise)" } else { "NEW_KEY (group)" };
        self.run(Command::NewKey, false, what, |b| {
            let mut b = b.u32(attr::IFINDEX, ifindex.raw());
            if let Some(peer) = key.peer {
                b = b.attr(attr::MAC, &peer.0);
            }
            b.nest(attr::KEY, |n| {
                let n = n
                    .attr(key_attr::DATA, key.data)
                    .u8(key_attr::IDX, key.index)
                    .u32(key_attr::CIPHER, key.cipher)
                    .u32(key_attr::TYPE, if key.peer.is_some() { KEYTYPE_PAIRWISE } else { KEYTYPE_GROUP });
                match key.seq {
                    Some(seq) => n.attr(key_attr::SEQ, seq),
                    None => n,
                }
            })
        })
        .map(drop)
    }

    /// Make group key `index` the default for multicast (`NL80211_CMD_SET_KEY`).
    pub fn set_default_multicast_key(&mut self, ifindex: IfIndex, index: u8) -> Result<(), ClientError> {
        self.run(Command::SetKey, false, "SET_KEY (default multicast)", |b| {
            b.u32(attr::IFINDEX, ifindex.raw()).nest(attr::KEY, |n| {
                n.u8(key_attr::IDX, index)
                    .flag(key_attr::DEFAULT)
                    .nest(key_attr::DEFAULT_TYPES, |t| t.flag(key_attr::DEFAULT_TYPE_MULTICAST))
            })
        })
        .map(drop)
    }

    /// Make integrity group key `index` the default for management frames (`NL80211_CMD_SET_KEY`).
    pub fn set_default_mgmt_key(&mut self, ifindex: IfIndex, index: u8) -> Result<(), ClientError> {
        self.run(Command::SetKey, false, "SET_KEY (default management)", |b| {
            b.u32(attr::IFINDEX, ifindex.raw())
                .nest(attr::KEY, |n| n.u8(key_attr::IDX, index).flag(key_attr::DEFAULT_MGMT))
        })
        .map(drop)
    }

    /// Open the 802.1X port for `peer` (`NL80211_CMD_SET_STATION` with `STA_FLAG_AUTHORIZED`): the AP
    /// on a station interface, or a station on an AP interface.
    pub fn authorize(&mut self, ifindex: IfIndex, peer: MacAddr) -> Result<(), ClientError> {
        self.run(Command::SetStation, false, "SET_STATION (authorized)", |b| {
            b.u32(attr::IFINDEX, ifindex.raw())
                .attr(attr::MAC, &peer.0)
                .attr(attr::STA_FLAGS2, &sta_flags(STA_FLAG_AUTHORIZED, STA_FLAG_AUTHORIZED))
        })
        .map(drop)
    }

    /// Admit an associated station on an AP interface, not yet authorized.
    pub fn new_station(&mut self, ifindex: IfIndex, station: &StationParams<'_>) -> Result<(), ClientError> {
        let state = STA_FLAG_AUTHENTICATED | STA_FLAG_ASSOCIATED;
        let mfp = if station.mfp { STA_FLAG_MFP } else { 0 };
        self.run(Command::NewStation, false, "NEW_STATION", |b| {
            b.u32(attr::IFINDEX, ifindex.raw())
                .attr(attr::MAC, &station.mac.0)
                .u16(attr::STA_AID, station.aid)
                .u16(attr::STA_LISTEN_INTERVAL, station.listen_interval)
                .u16(attr::STA_CAPABILITY, station.capability)
                .attr(attr::STA_SUPPORTED_RATES, station.supported_rates)
                .attr(attr::STA_FLAGS2, &sta_flags(state | STA_FLAG_AUTHORIZED | STA_FLAG_MFP, state | mfp))
        })
        .map(drop)
    }

    /// Remove a station from an AP interface (`NL80211_CMD_DEL_STATION`), so that it can be admitted
    /// again. Nothing is transmitted: the access point tells the station itself.
    pub fn del_station(&mut self, ifindex: IfIndex, peer: MacAddr) -> Result<(), ClientError> {
        self.run(Command::DelStation, false, "DEL_STATION", |b| {
            b.u32(attr::IFINDEX, ifindex.raw()).attr(attr::MAC, &peer.0)
        })
        .map(drop)
    }

    /// Have management frames of `frame_type` (the frame control field) delivered to this socket.
    pub fn register_frame(&mut self, ifindex: IfIndex, frame_type: u16) -> Result<(), ClientError> {
        self.run(Command::RegisterFrame, false, "REGISTER_FRAME", |b| {
            b.u32(attr::IFINDEX, ifindex.raw()).u16(attr::FRAME_TYPE, frame_type).attr(attr::FRAME_MATCH, &[])
        })
        .map(drop)
    }

    /// Transmit a management frame (`NL80211_CMD_FRAME`); its cookie names the transmit status.
    pub fn send_frame(&mut self, ifindex: IfIndex, frame: &[u8]) -> Result<u64, ClientError> {
        let replies =
            self.run(Command::Frame, false, "FRAME", |b| b.u32(attr::IFINDEX, ifindex.raw()).attr(attr::FRAME, frame))?;
        for reply in &replies {
            let (_, message) = nl80211::parse(&reply.payload).context(DecodeSnafu { what: "FRAME" })?;
            for a in netlink::attrs(message.attrs) {
                let a = a.context(DecodeSnafu { what: "FRAME" })?;
                if a.kind == attr::COOKIE {
                    return a.u64().context(DecodeSnafu { what: "FRAME" });
                }
            }
        }
        Ok(0)
    }

    /// Every wireless interface with a netdev (`NL80211_CMD_GET_INTERFACE` dump).
    pub fn interfaces(&mut self) -> Result<Vec<Interface>, ClientError> {
        let replies = self.run(Command::GetInterface, true, "GET_INTERFACE", |b| b)?;
        let mut found = Vec::new();
        for reply in &replies {
            let (_, message) = nl80211::parse(&reply.payload).context(DecodeSnafu { what: "GET_INTERFACE" })?;
            if let Some(interface) = Interface::parse(&message).context(DecodeSnafu { what: "GET_INTERFACE" })? {
                found.push(interface);
            }
        }
        Ok(found)
    }

    /// Change an interface's type (`NL80211_CMD_SET_INTERFACE`).
    pub fn set_iftype(&mut self, ifindex: IfIndex, iftype: IfType) -> Result<(), ClientError> {
        self.run(Command::SetInterface, false, "SET_INTERFACE", |b| {
            b.u32(attr::IFINDEX, ifindex.raw()).u32(attr::IFTYPE, iftype.raw())
        })
        .map(drop)
    }

    /// Start beaconing an open network on an AP interface (`NL80211_CMD_START_AP`).
    ///
    /// With an RSN element the network is WPA2-PSK or WPA3-SAE (`akms`): the element is the beacon's
    /// tail, the privacy bit is set, and EAPOL goes over nl80211 to this socket, which owns the AP.
    pub fn start_ap(&mut self, ifindex: IfIndex, mac: MacAddr, ap: &ApConfig<'_>) -> Result<(), ClientError> {
        let head = beacon_head(mac, ap);
        self.run(Command::StartAp, false, "START_AP", |b| {
            let b = b
                .u32(attr::IFINDEX, ifindex.raw())
                .attr(attr::BEACON_HEAD, &head)
                .attr(attr::BEACON_TAIL, ap.rsn.unwrap_or_default())
                .u32(attr::BEACON_INTERVAL, u32::from(ap.beacon_interval))
                .u32(attr::DTIM_PERIOD, 1)
                .attr(attr::SSID, ap.ssid)
                .u32(attr::HIDDEN_SSID, if ap.hidden { HIDDEN_SSID_ZERO_LEN } else { 0 })
                .u32(attr::AUTH_TYPE, 0)
                .u32(attr::WIPHY_FREQ, ap.frequency.0)
                .u32(attr::WIPHY_CHANNEL_TYPE, 0);
            if ap.rsn.is_none() {
                return b;
            }
            let akms: Vec<u8> = ap.akms.iter().flat_map(|akm| akm.to_ne_bytes()).collect();
            b.flag(attr::PRIVACY)
                .u32(attr::WPA_VERSIONS, WPA_VERSION_2)
                .attr(attr::AKM_SUITES, &akms)
                .u32(attr::CIPHER_SUITES_PAIRWISE, CIPHER_CCMP)
                .u32(attr::CIPHER_SUITE_GROUP, CIPHER_CCMP)
                .flag(attr::CONTROL_PORT)
                .flag(attr::CONTROL_PORT_OVER_NL80211)
                .flag(attr::SOCKET_OWNER)
        })
        .map(drop)
    }

    /// Ask for a scan of every channel (`NL80211_CMD_TRIGGER_SCAN`), or of `frequencies` when given.
    /// With `ssids` the scan is active and probes for each name (`NL80211_ATTR_SCAN_SSIDS`), and for
    /// the wildcard besides, so that networks that broadcast their names are found as before.
    pub fn trigger_scan(
        &mut self,
        ifindex: IfIndex,
        frequencies: &[Frequency],
        ssids: &[&[u8]],
    ) -> Result<(), ClientError> {
        self.run(Command::TriggerScan, false, "TRIGGER_SCAN", |b| {
            let mut b = b.u32(attr::IFINDEX, ifindex.raw());
            if !ssids.is_empty() {
                b = b.nest(attr::SCAN_SSIDS, |mut nest| {
                    for (n, ssid) in (1_u16..).zip(ssids.iter().copied().chain(core::iter::once(&[][..]))) {
                        nest = nest.attr(n, ssid);
                    }
                    nest
                });
            }
            if frequencies.is_empty() {
                return b;
            }
            b.nest(attr::SCAN_FREQUENCIES, |mut nest| {
                for (n, frequency) in (1_u16..).zip(frequencies) {
                    nest = nest.u32(n, frequency.0);
                }
                nest
            })
        })
        .map(drop)
    }

    /// Wait on the scan group for `ifindex`'s scan to end, until `deadline` on `clock`.
    pub fn wait_scan(
        &mut self,
        ifindex: IfIndex,
        deadline: Instant,
        clock: &dyn Fn() -> Instant,
    ) -> Result<Option<ScanEnd>, ClientError> {
        const WHAT: &str = "the scan notification";
        loop {
            let left = deadline.saturating_duration_since(clock());
            if left.is_zero() {
                return Ok(None);
            }
            let Some(len) =
                self.events.recv(&mut self.buf, left).map_err(|errno| ClientError::Recv { what: WHAT, errno })?
            else {
                return Ok(None);
            };
            for message in netlink::messages(&self.buf[..len]) {
                let message = message.context(DecodeSnafu { what: WHAT })?;
                if message.kind != self.family.id.message_type() {
                    continue;
                }
                let (cmd, genl) = nl80211::parse(message.payload).context(DecodeSnafu { what: WHAT })?;
                let about = nl80211::ifindex(&genl).context(DecodeSnafu { what: WHAT })?;
                log::debug!("scan group: {cmd:?} for ifindex {about:?}");
                match cmd {
                    Command::NewScanResults if about == Some(ifindex) => return Ok(Some(ScanEnd::Done)),
                    Command::ScanAborted if about == Some(ifindex) => return Ok(Some(ScanEnd::Aborted)),
                    _ => {},
                }
            }
        }
    }

    /// The BSSes cfg80211 holds for `ifindex` (`NL80211_CMD_GET_SCAN` dump).
    pub fn scan_results(&mut self, ifindex: IfIndex) -> Result<Vec<Bss>, ClientError> {
        let replies = self.run(Command::GetScan, true, "GET_SCAN", |b| b.u32(attr::IFINDEX, ifindex.raw()))?;
        bss_list(&replies)
    }
}

/// The Ethernet type of EAPOL.
const ETH_P_PAE: u16 = 0x888e;

/// `struct nl80211_sta_flag_update`.
fn sta_flags(mask: u32, set: u32) -> [u8; 8] {
    let mut bytes = [0; 8];
    bytes[..4].copy_from_slice(&mask.to_ne_bytes());
    bytes[4..].copy_from_slice(&set.to_ne_bytes());
    bytes
}

fn bss_list(replies: &[Reply]) -> Result<Vec<Bss>, ClientError> {
    let mut found = Vec::new();
    for reply in replies {
        let (_, message) = nl80211::parse(&reply.payload).context(DecodeSnafu { what: "GET_SCAN" })?;
        if let Some(bss) = Bss::parse(&message).context(DecodeSnafu { what: "GET_SCAN" })? {
            found.push(bss);
        }
    }
    Ok(found)
}

/// Send `message` and collect the replies of sequence `seq`: up to the ack, or for a dump up to
/// `NLMSG_DONE`, over as many datagrams as it takes. Protocol messages of other sequences come back
/// second, in order.
fn exchange<S: Socket>(
    socket: &mut S,
    buf: &mut [u8],
    message: &[u8],
    seq: u32,
    dump: bool,
    what: &'static str,
) -> Result<(Vec<Reply>, Vec<Reply>), ClientError> {
    socket.send(message).map_err(|errno| ClientError::Send { what, errno })?;
    let mut collector = Collector { seq, dump, what, replies: Vec::new(), stashed: Vec::new() };
    loop {
        let len = socket
            .recv(buf, REPLY_TIMEOUT)
            .map_err(|errno| ClientError::Recv { what, errno })?
            .context(TimeoutSnafu { what, timeout: REPLY_TIMEOUT })?;
        if collector.datagram(&buf[..len])? {
            return Ok((collector.replies, collector.stashed));
        }
    }
}

struct Collector {
    seq: u32,
    dump: bool,
    what: &'static str,
    replies: Vec<Reply>,
    stashed: Vec<Reply>,
}

impl Collector {
    /// Take one datagram's messages; true once the exchange is complete.
    fn datagram(&mut self, bytes: &[u8]) -> Result<bool, ClientError> {
        let what = self.what;
        for message in netlink::messages(bytes) {
            let message = message.context(DecodeSnafu { what })?;
            if message.seq != self.seq {
                if let MessageType::Protocol(_) = message.kind {
                    self.stashed.push(Reply { payload: message.payload.to_vec() });
                } else {
                    log::debug!("{what}: skipped a message of sequence {}", message.seq);
                }
                continue;
            }
            match message.kind {
                MessageType::Error => {
                    let error = ErrorMessage::parse(&message).context(DecodeSnafu { what })?;
                    if error.code != 0 {
                        return Err(ClientError::Kernel { what, errno: Errno(error.code), text: error.text });
                    }
                    if !self.dump {
                        return Ok(true);
                    }
                },
                MessageType::Done if self.dump => {
                    let code = message.payload.get(..4).map_or(0, |b| i32::from_ne_bytes([b[0], b[1], b[2], b[3]]));
                    if code != 0 {
                        return Err(ClientError::Kernel { what, errno: Errno(code), text: None });
                    }
                    return Ok(true);
                },
                MessageType::Protocol(_) => {
                    self.replies.push(Reply { payload: message.payload.to_vec() });
                },
                _ => {},
            }
        }
        Ok(false)
    }
}

/// A beacon's fixed part, as hostapd builds it: the management header, timestamp, interval,
/// capability (ESS, and privacy for an RSN network), then SSID, supported rates and DS Parameter Set elements.
fn beacon_head(mac: MacAddr, ap: &ApConfig<'_>) -> Vec<u8> {
    let mut head = Vec::with_capacity(64 + ap.ssid.len());
    head.extend_from_slice(&[0x80, 0x00, 0x00, 0x00]);
    head.extend_from_slice(&[0xff; 6]);
    head.extend_from_slice(&mac.0);
    head.extend_from_slice(&mac.0);
    head.extend_from_slice(&[0, 0]);
    head.extend_from_slice(&[0; 8]);
    head.extend_from_slice(&ap.beacon_interval.to_le_bytes());
    let capability: u16 = if ap.rsn.is_some() { 0x0011 } else { 0x0001 };
    head.extend_from_slice(&capability.to_le_bytes());
    head.push(0);
    let shown = if ap.hidden { &[][..] } else { ap.ssid };
    head.push(shown.len() as u8);
    head.extend_from_slice(shown);
    head.extend_from_slice(&[1, 4, 0x82, 0x84, 0x8b, 0x96]);
    head.extend_from_slice(&[3, 1, ap.channel]);
    head
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;

    use assert2::{assert, check};

    use super::*;
    use crate::nl80211::{IfType, Mbm, Security};

    /// Datagrams captured from the lab: two `mac80211_hwsim` radios, the second an open AP.
    const INTERFACES: [&[u8]; 2] = [
        include_bytes!("../testdata/get-interface-dump-0.bin"),
        include_bytes!("../testdata/get-interface-dump-1.bin"),
    ];
    const SCAN_NOTIFICATION: &[u8] = include_bytes!("../testdata/scan-group-new-scan-results.bin");
    const SCAN_DUMP: [&[u8]; 2] =
        [include_bytes!("../testdata/get-scan-dump-0.bin"), include_bytes!("../testdata/get-scan-dump-1.bin")];

    fn collect(seq: u32, datagrams: &[&[u8]]) -> (Vec<Reply>, Vec<bool>) {
        let mut collector = Collector { seq, dump: true, what: "test", replies: Vec::new(), stashed: Vec::new() };
        let done = datagrams.iter().map(|datagram| collector.datagram(datagram).unwrap()).collect();
        (collector.replies, done)
    }

    #[test]
    fn an_interface_dump_spans_two_datagrams_and_ends_at_done() {
        let (replies, done) = collect(2, &INTERFACES);
        check!(done == [false, true]);
        let interfaces: Vec<Interface> = replies
            .iter()
            .filter_map(|reply| Interface::parse(&nl80211::parse(&reply.payload).unwrap().1).unwrap())
            .collect();
        let names: Vec<&str> = interfaces.iter().map(|i| i.name.as_str()).collect();
        check!(names == ["wlan1", "wlan0"]);
        check!(interfaces.iter().map(|i| i.ifindex.raw()).collect::<Vec<_>>() == [3, 2]);
        check!(interfaces.iter().all(|i| i.iftype == IfType::Station));
        check!(interfaces[0].mac.to_string() == "02:00:00:00:01:00");
    }

    #[test]
    fn a_scan_dump_skips_the_notification_of_another_sequence_and_decodes_the_bss() {
        let (replies, done) = collect(7, &[SCAN_NOTIFICATION, SCAN_DUMP[0], SCAN_DUMP[1]]);
        check!(done == [false, false, true]);
        assert!(let Ok(bsses) = bss_list(&replies));
        check!(bsses.len() == 1);
        check!(bsses[0].bssid.to_string() == "02:00:00:00:01:00");
        check!(bsses[0].frequency == Frequency(2412));
        check!(bsses[0].signal == Some(Mbm(-3000)));
        check!(bsses[0].ssid == b"ferrite-lab");
        check!(bsses[0].security == Security::Open);
    }

    #[test]
    fn the_scan_notification_names_its_command_and_interface() {
        let message = netlink::messages(SCAN_NOTIFICATION).next().unwrap().unwrap();
        check!(message.seq == 0);
        let (cmd, genl) = nl80211::parse(message.payload).unwrap();
        check!(cmd == Command::NewScanResults);
        check!(nl80211::ifindex(&genl).unwrap() == IfIndex::new(2));
    }

    #[test]
    fn a_notification_between_a_request_and_its_ack_is_kept_for_next_event() {
        let connect = include_bytes!("../testdata/connect-event.bin");
        let mut ack = Vec::new();
        ack.extend_from_slice(&36_u32.to_ne_bytes());
        ack.extend_from_slice(&2_u16.to_ne_bytes());
        ack.extend_from_slice(&0x100_u16.to_ne_bytes());
        ack.extend_from_slice(&7_u32.to_ne_bytes());
        ack.extend_from_slice(&5_u32.to_ne_bytes());
        ack.extend_from_slice(&0_i32.to_ne_bytes());
        ack.extend_from_slice(&[0; 16]);
        let mut collector =
            Collector { seq: 7, dump: false, what: "NEW_KEY", replies: Vec::new(), stashed: Vec::new() };
        check!(collector.datagram(connect).unwrap() == false);
        check!(collector.datagram(&ack).unwrap());
        check!(collector.replies.is_empty());
        check!(collector.stashed.len() == 1);
        check!(nl80211::parse(&collector.stashed[0].payload).unwrap().0 == Command::Connect);
    }

    #[test]
    fn a_kernel_error_ends_the_exchange_with_its_errno() {
        // NLMSG_ERROR, capped, -EBUSY, echoing a request header of sequence 9.
        let mut datagram = Vec::new();
        datagram.extend_from_slice(&36_u32.to_ne_bytes());
        datagram.extend_from_slice(&2_u16.to_ne_bytes());
        datagram.extend_from_slice(&0x100_u16.to_ne_bytes());
        datagram.extend_from_slice(&9_u32.to_ne_bytes());
        datagram.extend_from_slice(&5_u32.to_ne_bytes());
        datagram.extend_from_slice(&(-16_i32).to_ne_bytes());
        datagram.extend_from_slice(&[0; 16]);
        let mut collector =
            Collector { seq: 9, dump: false, what: "TRIGGER_SCAN", replies: Vec::new(), stashed: Vec::new() };
        assert!(let Err(ClientError::Kernel { errno: Errno(-16), .. }) = collector.datagram(&datagram));
    }
}
