//! What the "mlme" group and a connection owner's socket receive: connect results, disconnects,
//! EAPOL frames over the control port, received management frames and their transmit status.

use alloc::vec::Vec;
use core::fmt;

use crate::genl::GenlMessage;
use crate::netlink::{self, DecodeError};
use crate::nl80211::{Command, Frequency, IfIndex, MacAddr, attr};

/// The multicast group connect and disconnect events go to.
pub const MLME_GROUP: &str = "mlme";

macro_rules! wire_code {
    ($(#[$meta:meta])* $name:ident { $($variant:ident = $value:literal),* $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum $name {
            $($variant,)*
            Unknown(u16),
        }

        impl $name {
            pub const fn from_raw(raw: u16) -> Self {
                match raw {
                    $($value => Self::$variant,)*
                    other => Self::Unknown(other),
                }
            }

            pub const fn raw(self) -> u16 {
                match self {
                    $(Self::$variant => $value,)*
                    Self::Unknown(raw) => raw,
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                match self {
                    Self::Unknown(raw) => write!(f, "{raw} (unknown)"),
                    known => write!(f, "{} ({known:?})", known.raw()),
                }
            }
        }
    };
}

wire_code! {
    /// An IEEE 802.11 status code, as an authentication or association response carries it.
    StatusCode {
        Success = 0,
        UnspecifiedFailure = 1,
        CapabilitiesUnsupported = 10,
        AssocDenied = 12,
        AuthAlgorithmUnsupported = 13,
        ChallengeFailure = 15,
        AuthTimeout = 16,
        ApFull = 17,
        BasicRatesUnsupported = 18,
        RejectedTemporarily = 30,
        RobustManagementPolicyViolation = 31,
        InvalidElement = 40,
        InvalidGroupCipher = 41,
        InvalidPairwiseCipher = 42,
        InvalidAkm = 43,
        UnsupportedRsnVersion = 44,
        InvalidRsnCapabilities = 45,
        CipherSuiteRejected = 46,
    }
}

wire_code! {
    /// An IEEE 802.11 reason code, as a deauthentication or disassociation carries it.
    ReasonCode {
        Unspecified = 1,
        PreviousAuthNotValid = 2,
        DeauthLeaving = 3,
        Inactivity = 4,
        ApBusy = 5,
        Class2FromUnauthenticated = 6,
        Class3FromUnassociated = 7,
        StationLeft = 8,
        AssocWithoutAuth = 9,
        MicFailure = 14,
        FourWayHandshakeTimeout = 15,
        GroupKeyHandshakeTimeout = 16,
        ElementDiffers = 17,
        Ieee8021xFailed = 23,
    }
}

/// One notification, decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The result of a connect: `NL80211_CMD_CONNECT` sent to the mlme group.
    Connect {
        ifindex: IfIndex,
        bssid: Option<MacAddr>,
        status: StatusCode,
        timed_out: bool,
    },
    Disconnect {
        ifindex: IfIndex,
        reason: Option<ReasonCode>,
        by_ap: bool,
    },
    /// An EAPOL frame, from its 802.1X header on.
    ControlPortFrame {
        ifindex: IfIndex,
        source: MacAddr,
        ethertype: u16,
        frame: Vec<u8>,
        unencrypted: bool,
    },
    /// A received management frame this socket registered for, from its 802.11 header on.
    Frame {
        ifindex: IfIndex,
        frequency: Option<Frequency>,
        frame: Vec<u8>,
    },
    FrameTxStatus {
        ifindex: IfIndex,
        cookie: u64,
        ack: bool,
    },
    /// An authentication frame from the access point (`frame` from its 802.11 header on), or its
    /// timeout (`frame` empty, `timed_out`): `NL80211_CMD_AUTHENTICATE` on the mlme group, which is how
    /// userspace SAE sees the access point's commit and confirm.
    Authenticate {
        ifindex: IfIndex,
        frame: Vec<u8>,
        timed_out: bool,
    },
    Other {
        cmd: Command,
        ifindex: Option<IfIndex>,
    },
}

impl Event {
    pub fn parse(cmd: Command, message: &GenlMessage<'_>) -> Result<Self, DecodeError> {
        let mut ifindex = None;
        let (mut mac, mut status, mut reason, mut frame, mut frequency, mut cookie) = (None, None, None, None, None, 0);
        let (mut flag_a, mut flag_b, mut ethertype) = (false, false, 0);
        for a in netlink::attrs(message.attrs) {
            let a = a?;
            match a.kind {
                attr::IFINDEX => ifindex = IfIndex::new(a.u32()?),
                attr::MAC => mac = Some(MacAddr(a.array()?)),
                attr::STATUS_CODE => status = Some(a.u16()?),
                attr::REASON_CODE => reason = Some(a.u16()?),
                attr::FRAME => frame = Some(a.payload.to_vec()),
                attr::WIPHY_FREQ => frequency = Some(Frequency(a.u32()?)),
                attr::COOKIE => cookie = a.u64()?,
                attr::CONTROL_PORT_ETHERTYPE => ethertype = a.u16()?,
                attr::TIMED_OUT | attr::DISCONNECTED_BY_AP | attr::ACK => flag_a = true,
                attr::CONTROL_PORT_NO_ENCRYPT => flag_b = true,
                _ => {},
            }
        }
        let Some(ifindex) = ifindex else { return Ok(Self::Other { cmd, ifindex: None }) };
        Ok(match (cmd, mac, frame) {
            (Command::Connect, bssid, _) => Self::Connect {
                ifindex,
                bssid,
                status: StatusCode::from_raw(status.unwrap_or(StatusCode::UnspecifiedFailure.raw())),
                timed_out: flag_a,
            },
            (Command::Disconnect, _, _) => {
                Self::Disconnect { ifindex, reason: reason.map(ReasonCode::from_raw), by_ap: flag_a }
            },
            (Command::ControlPortFrame, Some(source), Some(frame)) => {
                Self::ControlPortFrame { ifindex, source, ethertype, frame, unencrypted: flag_b }
            },
            (Command::Frame, _, Some(frame)) => Self::Frame { ifindex, frequency, frame },
            (Command::FrameTxStatus, _, _) => Self::FrameTxStatus { ifindex, cookie, ack: flag_a },
            (Command::Authenticate, _, frame) => {
                Self::Authenticate { ifindex, timed_out: frame.is_none() && flag_a, frame: frame.unwrap_or_default() }
            },
            _ => Self::Other { cmd, ifindex: Some(ifindex) },
        })
    }

    pub const fn ifindex(&self) -> Option<IfIndex> {
        match self {
            Self::Connect { ifindex, .. }
            | Self::Disconnect { ifindex, .. }
            | Self::ControlPortFrame { ifindex, .. }
            | Self::Frame { ifindex, .. }
            | Self::Authenticate { ifindex, .. }
            | Self::FrameTxStatus { ifindex, .. } => Some(*ifindex),
            Self::Other { ifindex, .. } => *ifindex,
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;

    use assert2::check;
    use rstest::rstest;

    use super::*;
    use crate::genl::FamilyId;
    use crate::netlink::Flags;

    /// Datagrams captured from the lab: wlan0 (ifindex 2) connecting to wlan1's WPA2-PSK AP (ifindex 3).
    const CONNECT: &[u8] = include_bytes!("../testdata/connect-event.bin");
    const MESSAGE_1: &[u8] = include_bytes!("../testdata/control-port-message-1.bin");
    const MESSAGE_3: &[u8] = include_bytes!("../testdata/control-port-message-3.bin");
    const DISCONNECT: &[u8] = include_bytes!("../testdata/disconnect-event.bin");
    const ASSOC_REQUEST: &[u8] = include_bytes!("../testdata/ap-frame-assoc-request.bin");
    const TX_STATUS: &[u8] = include_bytes!("../testdata/ap-frame-tx-status.bin");

    const AP: MacAddr = MacAddr([2, 0, 0, 0, 1, 0]);

    fn event(datagram: &[u8]) -> Event {
        let message = netlink::messages(datagram).next().unwrap().unwrap();
        check!(message.seq == 0);
        let (cmd, genl) = crate::nl80211::parse(message.payload).unwrap();
        Event::parse(cmd, &genl).unwrap()
    }

    #[test]
    fn the_connect_event_carries_success_and_the_bssid() {
        check!(
            event(CONNECT)
                == Event::Connect {
                    ifindex: IfIndex::new(2).unwrap(),
                    bssid: Some(AP),
                    status: StatusCode::Success,
                    timed_out: false,
                }
        );
    }

    #[test]
    fn eapol_messages_arrive_unencrypted_from_the_ap_with_their_8021x_header() {
        for (datagram, key_info) in [(MESSAGE_1, 0x008a_u16), (MESSAGE_3, 0x13ca)] {
            let Event::ControlPortFrame { ifindex, source, ethertype, frame, unencrypted } = event(datagram) else {
                panic!("not a control port frame");
            };
            check!((ifindex.raw(), source, ethertype, unencrypted) == (2, AP, 0x888e, true));
            check!(frame[..2] == [2, 3]);
            check!(u16::from_be_bytes([frame[5], frame[6]]) == key_info);
        }
    }

    #[test]
    fn a_local_disconnect_names_its_reason_and_not_the_ap() {
        check!(
            event(DISCONNECT)
                == Event::Disconnect {
                    ifindex: IfIndex::new(2).unwrap(),
                    reason: Some(ReasonCode::DeauthLeaving),
                    by_ap: false,
                }
        );
    }

    #[test]
    fn the_ap_receives_the_association_request_and_an_acked_transmit_status() {
        let Event::Frame { ifindex, frequency, frame } = event(ASSOC_REQUEST) else { panic!("not a frame") };
        check!((ifindex.raw(), frequency) == (3, Some(Frequency(2412))));
        check!(frame[0] == 0x00);
        check!(frame[10..16] == [2, 0, 0, 0, 0, 0]);
        assert2::assert!(let Event::FrameTxStatus { ack: true, .. } = event(TX_STATUS));
    }

    #[test]
    fn codes_outside_the_table_keep_their_number() {
        check!(StatusCode::from_raw(0) == StatusCode::Success);
        check!(StatusCode::from_raw(77) == StatusCode::Unknown(77));
        check!(ReasonCode::from_raw(15).to_string() == "15 (FourWayHandshakeTimeout)");
        check!(ReasonCode::from_raw(99).to_string() == "99 (unknown)");
    }

    fn built(cmd: Command, attrs: impl FnOnce(netlink::Builder) -> netlink::Builder) -> Event {
        let family = FamilyId::new(30).unwrap();
        let bytes = attrs(crate::nl80211::request(family, Flags::ACK, 0, cmd)).finish();
        let message = netlink::messages(&bytes).next().unwrap().unwrap();
        let (cmd, genl) = crate::nl80211::parse(message.payload).unwrap();
        Event::parse(cmd, &genl).unwrap()
    }

    #[rstest]
    #[case::the_access_points_frame(Some(&[0xb0, 0, 1, 2][..]), false)]
    #[case::a_timeout(None, true)]
    fn an_authentication_event_carries_the_frame_or_the_timeout(#[case] frame: Option<&[u8]>, #[case] timed_out: bool) {
        let event = built(Command::Authenticate, |b| {
            let b = b.u32(attr::IFINDEX, 2).attr(attr::MAC, &AP.0);
            match frame {
                Some(frame) => b.attr(attr::FRAME, frame),
                None => b.flag(attr::TIMED_OUT),
            }
        });
        check!(
            event
                == Event::Authenticate {
                    ifindex: IfIndex::new(2).unwrap(),
                    frame: frame.unwrap_or_default().to_vec(),
                    timed_out
                }
        );
    }
}
