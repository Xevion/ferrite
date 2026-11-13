//! Generic netlink: the `genlmsghdr` after the netlink header, and the controller family that maps a
//! family name to its message type and multicast groups.

use alloc::string::String;
use alloc::vec::Vec;

use snafu::{OptionExt, Snafu, ensure};

use crate::netlink::{self, Builder, DecodeError, Flags, MessageType};

/// `GENL_HDRLEN`: `cmd: u8, version: u8, reserved: u16`.
pub const HEADER_LEN: usize = 4;
/// `GENL_ID_CTRL`, the controller's fixed family id.
pub const CONTROLLER: FamilyId = FamilyId(0x10);
/// `GENL_MIN_ID`: lower message types are netlink's own.
const MIN_ID: u16 = 0x10;

const CTRL_CMD_NEWFAMILY: u8 = 1;
const CTRL_CMD_GETFAMILY: u8 = 3;
const CTRL_ATTR_FAMILY_ID: u16 = 1;
const CTRL_ATTR_FAMILY_NAME: u16 = 2;
const CTRL_ATTR_MCAST_GROUPS: u16 = 7;
const CTRL_ATTR_MCAST_GRP_NAME: u16 = 1;
const CTRL_ATTR_MCAST_GRP_ID: u16 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Snafu)]
pub enum GenlError {
    #[snafu(display("family message: {source}"))]
    Decode { source: DecodeError },
    #[snafu(display("family message is command {cmd}, not NEWFAMILY"))]
    NotNewFamily { cmd: u8 },
    #[snafu(display("family {name:?} has no valid id"))]
    NoId { name: String },
    #[snafu(display("multicast group {name:?} has id 0"))]
    GroupId { name: String },
}

impl From<DecodeError> for GenlError {
    fn from(source: DecodeError) -> Self {
        Self::Decode { source }
    }
}

/// A generic netlink family's message type: at least `GENL_MIN_ID`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FamilyId(u16);

impl FamilyId {
    pub const fn new(raw: u16) -> Option<Self> {
        if raw >= MIN_ID { Some(Self(raw)) } else { None }
    }

    pub const fn raw(self) -> u16 {
        self.0
    }

    pub const fn message_type(self) -> MessageType {
        MessageType::Protocol(self.0)
    }
}

/// A multicast group id, which `NETLINK_ADD_MEMBERSHIP` takes; never 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GroupId(u32);

impl GroupId {
    pub const fn new(raw: u32) -> Option<Self> {
        if raw == 0 { None } else { Some(Self(raw)) }
    }

    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// The generic netlink header and the attributes after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GenlMessage<'a> {
    pub cmd: u8,
    pub version: u8,
    pub attrs: &'a [u8],
}

impl<'a> GenlMessage<'a> {
    pub fn parse(payload: &'a [u8]) -> Result<Self, DecodeError> {
        ensure!(payload.len() >= HEADER_LEN, netlink::ShortSnafu { len: payload.len(), want: HEADER_LEN });
        Ok(Self { cmd: payload[0], version: payload[1], attrs: &payload[HEADER_LEN..] })
    }
}

/// A request to `family` running `cmd`.
pub fn request(family: FamilyId, flags: Flags, seq: u32, cmd: u8, version: u8) -> Builder {
    Builder::new(family.message_type(), Flags::REQUEST.with(flags), seq).raw(&[cmd, version, 0, 0])
}

/// `CTRL_CMD_GETFAMILY` for the family called `name`.
pub fn get_family(seq: u32, name: &str) -> Vec<u8> {
    request(CONTROLLER, Flags::ACK, seq, CTRL_CMD_GETFAMILY, 1).str(CTRL_ATTR_FAMILY_NAME, name).finish()
}

/// What the controller says about a family.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Family {
    pub name: String,
    pub id: FamilyId,
    pub groups: Vec<(String, GroupId)>,
}

impl Family {
    /// Parse a `CTRL_CMD_NEWFAMILY` reply.
    pub fn parse(message: &GenlMessage<'_>) -> Result<Self, GenlError> {
        ensure!(message.cmd == CTRL_CMD_NEWFAMILY, NotNewFamilySnafu { cmd: message.cmd });
        let (mut name, mut id, mut groups) = (String::new(), None, Vec::new());
        for attr in netlink::attrs(message.attrs) {
            let attr = attr?;
            match attr.kind {
                CTRL_ATTR_FAMILY_NAME => name = String::from(attr.str()?),
                CTRL_ATTR_FAMILY_ID => id = FamilyId::new(attr.u16()?),
                CTRL_ATTR_MCAST_GROUPS => {
                    for group in attr.attrs() {
                        let (mut group_name, mut group_id) = (String::new(), 0);
                        for field in group?.attrs() {
                            let field = field?;
                            match field.kind {
                                CTRL_ATTR_MCAST_GRP_NAME => group_name = String::from(field.str()?),
                                CTRL_ATTR_MCAST_GRP_ID => group_id = field.u32()?,
                                _ => {},
                            }
                        }
                        let group_id = GroupId::new(group_id).context(GroupIdSnafu { name: group_name.clone() })?;
                        groups.push((group_name, group_id));
                    }
                },
                _ => {},
            }
        }
        let id = id.context(NoIdSnafu { name: name.clone() })?;
        Ok(Self { name, id, groups })
    }

    pub fn group(&self, name: &str) -> Option<GroupId> {
        self.groups.iter().find(|(group, _)| group == name).map(|(_, id)| *id)
    }
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};

    use super::*;

    #[test]
    fn the_nl80211_family_reply_from_the_lab_names_its_id_and_groups() {
        let datagram = include_bytes!("../testdata/getfamily-nl80211.bin");
        let message = netlink::messages(datagram).next().unwrap().unwrap();
        check!(message.kind == MessageType::Protocol(0x10));
        assert!(let Ok(family) = Family::parse(&GenlMessage::parse(message.payload).unwrap()));
        check!(family.name == "nl80211");
        check!(family.id.raw() == 19);
        let names: Vec<&str> = family.groups.iter().map(|(name, _)| name.as_str()).collect();
        check!(names.contains(&"scan"));
        check!(names.contains(&"mlme"));
        check!(family.group("scan").is_some_and(|id| id.raw() > 0));
    }
}
