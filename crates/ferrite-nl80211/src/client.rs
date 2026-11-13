//! An nl80211 client over any netlink socket, doing what `iw` does for a scan.
//!
//! Two sockets: requests and their replies go over one, and the other joins the "scan" multicast group,
//! so a dump's replies and the notifications never share a queue. A request without a dump asks for an
//! ack and ends at it; a dump ends at `NLMSG_DONE`. Messages of another sequence number are skipped.

use alloc::vec;
use alloc::vec::Vec;
use core::time::Duration;

use ferrite_hal::Instant;
use snafu::{OptionExt, ResultExt, Snafu};

use crate::genl::{self, Family, GenlError, GenlMessage, GroupId};
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

/// An open network to start on an AP interface.
pub struct ApConfig<'a> {
    pub ssid: &'a [u8],
    pub frequency: Frequency,
    /// In TU (1024 us); cfg80211 takes 10 to 10000.
    pub beacon_interval: u16,
    /// The 802.11 channel number the DS Parameter Set element names.
    pub channel: u8,
}

pub struct Client<S: Socket> {
    requests: S,
    events: S,
    family: Family,
    seq: u32,
    buf: Vec<u8>,
}

impl<S: Socket> Client<S> {
    /// Resolve nl80211 through the controller and join its scan group on `events`.
    pub fn connect(mut requests: S, mut events: S) -> Result<Self, ClientError> {
        let mut buf = vec![0; RECV_BUFFER];
        let replies =
            exchange(&mut requests, &mut buf, &genl::get_family(1, nl80211::FAMILY_NAME), 1, false, "GETFAMILY")?;
        let reply = replies.first().context(NoFamilySnafu { name: nl80211::FAMILY_NAME })?;
        let message = GenlMessage::parse(&reply.payload).context(DecodeSnafu { what: "GETFAMILY" })?;
        let family = Family::parse(&message).context(FamilySnafu { name: nl80211::FAMILY_NAME })?;
        let group = family
            .group(nl80211::SCAN_GROUP)
            .context(NoGroupSnafu { family: nl80211::FAMILY_NAME, group: nl80211::SCAN_GROUP })?;
        events.join(group).map_err(|errno| ClientError::Join { group: nl80211::SCAN_GROUP, errno })?;
        Ok(Self { requests, events, family, seq: 1, buf })
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
        exchange(&mut self.requests, &mut self.buf, &message, seq, dump, what)
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
    pub fn start_ap(&mut self, ifindex: IfIndex, mac: MacAddr, ap: &ApConfig<'_>) -> Result<(), ClientError> {
        let head = beacon_head(mac, ap);
        self.run(Command::StartAp, false, "START_AP", |b| {
            b.u32(attr::IFINDEX, ifindex.raw())
                .attr(attr::BEACON_HEAD, &head)
                .attr(attr::BEACON_TAIL, &[])
                .u32(attr::BEACON_INTERVAL, u32::from(ap.beacon_interval))
                .u32(attr::DTIM_PERIOD, 1)
                .attr(attr::SSID, ap.ssid)
                .u32(attr::HIDDEN_SSID, 0)
                .u32(attr::AUTH_TYPE, 0)
                .u32(attr::WIPHY_FREQ, ap.frequency.0)
                .u32(attr::WIPHY_CHANNEL_TYPE, 0)
        })
        .map(drop)
    }

    /// Ask for a scan of every channel (`NL80211_CMD_TRIGGER_SCAN`), or of `frequencies` when given.
    pub fn trigger_scan(&mut self, ifindex: IfIndex, frequencies: &[Frequency]) -> Result<(), ClientError> {
        self.run(Command::TriggerScan, false, "TRIGGER_SCAN", |b| {
            let b = b.u32(attr::IFINDEX, ifindex.raw());
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
/// `NLMSG_DONE`, over as many datagrams as it takes.
fn exchange<S: Socket>(
    socket: &mut S,
    buf: &mut [u8],
    message: &[u8],
    seq: u32,
    dump: bool,
    what: &'static str,
) -> Result<Vec<Reply>, ClientError> {
    socket.send(message).map_err(|errno| ClientError::Send { what, errno })?;
    let mut collector = Collector { seq, dump, what, replies: Vec::new() };
    loop {
        let len = socket
            .recv(buf, REPLY_TIMEOUT)
            .map_err(|errno| ClientError::Recv { what, errno })?
            .context(TimeoutSnafu { what, timeout: REPLY_TIMEOUT })?;
        if collector.datagram(&buf[..len])? {
            return Ok(collector.replies);
        }
    }
}

struct Collector {
    seq: u32,
    dump: bool,
    what: &'static str,
    replies: Vec<Reply>,
}

impl Collector {
    /// Take one datagram's messages; true once the exchange is complete.
    fn datagram(&mut self, bytes: &[u8]) -> Result<bool, ClientError> {
        let what = self.what;
        for message in netlink::messages(bytes) {
            let message = message.context(DecodeSnafu { what })?;
            if message.seq != self.seq {
                log::debug!("{what}: skipped a message of sequence {}", message.seq);
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
/// capability (ESS), then SSID, supported rates and DS Parameter Set elements.
fn beacon_head(mac: MacAddr, ap: &ApConfig<'_>) -> Vec<u8> {
    let mut head = Vec::with_capacity(64 + ap.ssid.len());
    head.extend_from_slice(&[0x80, 0x00, 0x00, 0x00]);
    head.extend_from_slice(&[0xff; 6]);
    head.extend_from_slice(&mac.0);
    head.extend_from_slice(&mac.0);
    head.extend_from_slice(&[0, 0]);
    head.extend_from_slice(&[0; 8]);
    head.extend_from_slice(&ap.beacon_interval.to_le_bytes());
    head.extend_from_slice(&0x0001_u16.to_le_bytes());
    head.push(0);
    head.push(ap.ssid.len() as u8);
    head.extend_from_slice(ap.ssid);
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
        let mut collector = Collector { seq, dump: true, what: "test", replies: Vec::new() };
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
        let mut collector = Collector { seq: 9, dump: false, what: "TRIGGER_SCAN", replies: Vec::new() };
        assert!(let Err(ClientError::Kernel { errno: Errno(-16), .. }) = collector.datagram(&datagram));
    }
}
