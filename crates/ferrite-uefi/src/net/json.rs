//! A small typed JSON writer and the document `/api/status` and the event stream send.
//!
//! Each type that appears in the document says how it is written through [`ToJson`], into any
//! `fmt::Write`: the status document goes to a buffer the server reuses and short replies to a
//! stack string. Numbers are integers only: 64-bit patterns and addresses go out as hex strings,
//! since a JavaScript number keeps only 53 bits.

use alloc::collections::VecDeque;
use core::fmt::{self, Write as _};
use core::time::Duration;

use ferrite_hal::Instant;
use ferrite_mem::Fault;
use ferrite_mem::analysis::Classification;
use ferrite_mem::eta::{Confidence, Eta, Position};

use ferrite_net::http::JsonEscape;
use ferrite_net::mdns;
use ferrite_net::{NetStatus, PortStatus};

use super::wifi::{WifiFacts, WifiView};
use crate::goal::Goal;
use crate::pace::Outlook;
use crate::report::{CpuInfo, FaultRecord, PassRecord, Snapshot, TestRecord};
use crate::sweep::SweepStatus;

pub trait ToJson {
    fn to_json(&self, out: &mut dyn fmt::Write);
}

/// A sink fails only when it has a fixed capacity and is full, which truncates the document
/// rather than leaving a state to recover from; the replies are sized to always fit.
fn push(out: &mut dyn fmt::Write, c: char) {
    let _ = out.write_char(c);
}

fn push_str(out: &mut dyn fmt::Write, s: &str) {
    let _ = out.write_str(s);
}

/// Writes the members of one object.
pub struct Object<'a> {
    out: &'a mut dyn fmt::Write,
    empty: bool,
}

impl<'a> Object<'a> {
    pub fn new(out: &'a mut dyn fmt::Write) -> Self {
        push(out, '{');
        Self { out, empty: true }
    }

    pub fn field(&mut self, key: &str, value: &(impl ToJson + ?Sized)) -> &mut Self {
        if !self.empty {
            push(self.out, ',');
        }
        self.empty = false;
        key.to_json(self.out);
        push(self.out, ':');
        value.to_json(self.out);
        self
    }

    pub fn finish(&mut self) {
        push(self.out, '}');
    }
}

/// A value written as a JSON string through its `Display`.
pub struct Text<T>(pub T);

impl<T: fmt::Display> ToJson for Text<T> {
    fn to_json(&self, out: &mut dyn fmt::Write) {
        push(out, '"');
        let _ = write!(JsonEscape(out), "{}", self.0);
        push(out, '"');
    }
}

/// A 64-bit value as a `0x`-prefixed hex string.
pub struct Hex(pub u64);

impl ToJson for Hex {
    fn to_json(&self, out: &mut dyn fmt::Write) {
        let _ = write!(out, "\"{:#x}\"", self.0);
    }
}

/// A duration as whole milliseconds.
pub struct Millis(pub Duration);

impl ToJson for Millis {
    fn to_json(&self, out: &mut dyn fmt::Write) {
        u64::try_from(self.0.as_millis()).unwrap_or(u64::MAX).to_json(out);
    }
}

/// A duration as whole seconds.
pub struct Secs(pub Duration);

impl ToJson for Secs {
    fn to_json(&self, out: &mut dyn fmt::Write) {
        self.0.as_secs().to_json(out);
    }
}

/// A time left: seconds and what the figure rests on.
fn eta_fields(o: &mut Object<'_>, eta: Eta) {
    let confidence = match eta.confidence {
        Confidence::Measured => "measured",
        Confidence::Estimated => "estimated",
    };
    o.field("left_s", &Secs(eta.remaining)).field("confidence", confidence);
}

impl ToJson for Goal {
    fn to_json(&self, out: &mut dyn fmt::Write) {
        Object::new(out)
            .field("passes", &self.passes.map(core::num::NonZeroU64::get))
            .field("time_s", &self.time.map(Secs))
            .field("stop_on_fault", &self.stop_on_fault)
            .field("text", &Text(self))
            .finish();
    }
}

impl ToJson for Position {
    fn to_json(&self, out: &mut dyn fmt::Write) {
        let mut o = Object::new(out);
        match *self {
            Self::Work { waits_passed, waits } => {
                o.field("kind", "work").field("waits_passed", &waits_passed).field("waits", &waits);
            },
            Self::Wait { nth, waits, left } => {
                o.field("kind", "wait").field("nth", &nth).field("waits", &waits).field("left_s", &Secs(left));
            },
        }
        o.finish();
    }
}

/// How the run is going as time, with the run measured against its goal as of `elapsed`.
struct ProgressDoc<'a> {
    outlook: &'a Outlook,
    elapsed: Duration,
    percent: u16,
}

impl ToJson for ProgressDoc<'_> {
    fn to_json(&self, out: &mut dyn fmt::Write) {
        let o = self.outlook;
        let overall = o.overall(self.elapsed, self.percent);
        let eta = |e: Option<Eta>| e.map(|e| Section(move |o: &mut Object<'_>| eta_fields(o, e)));
        Object::new(out)
            .field("settled", &o.settled)
            .field("reserve", &o.reserve)
            .field("rate", &o.rate)
            .field("position", &o.position)
            .field(
                "test",
                &Section(|s: &mut Object<'_>| {
                    s.field("left_s", &o.test_left.map(Secs)).field("permille", &o.test_permille);
                }),
            )
            .field("pass", &eta(o.pass))
            .field("pass_permille", &o.pass_permille)
            .field("cycle_s", &o.cycle.map(Secs))
            .field("cycle_left", &eta(o.cycle_left))
            .field(
                "overall",
                &Section(|s: &mut Object<'_>| {
                    s.field("elapsed_ms", &Millis(overall.elapsed))
                        .field("permille", &overall.fill)
                        .field("left", &eta(overall.left));
                }),
            )
            .field(
                "timings",
                &Section(|s: &mut Object<'_>| {
                    s.field("known", &o.known.0).field("of", &o.known.1);
                }),
            )
            .finish();
    }
}

/// The items of an iterator, as an array.
pub struct Each<F>(pub F);

impl<F, I> ToJson for Each<F>
where
    F: Fn() -> I,
    I: Iterator<Item: ToJson>,
{
    fn to_json(&self, out: &mut dyn fmt::Write) {
        push(out, '[');
        for (i, item) in (self.0)().enumerate() {
            if i > 0 {
                push(out, ',');
            }
            item.to_json(out);
        }
        push(out, ']');
    }
}

macro_rules! json_integer {
    ($($t:ty),*) => {$(
        impl ToJson for $t {
            fn to_json(&self, out: &mut dyn fmt::Write) {
                let _ = write!(out, "{self}");
            }
        }
    )*};
}

json_integer!(u16, u32, u64, u128, usize);

impl ToJson for bool {
    fn to_json(&self, out: &mut dyn fmt::Write) {
        push_str(out, if *self { "true" } else { "false" });
    }
}

impl ToJson for str {
    fn to_json(&self, out: &mut dyn fmt::Write) {
        Text(self).to_json(out);
    }
}

impl<T: ToJson + ?Sized> ToJson for &T {
    fn to_json(&self, out: &mut dyn fmt::Write) {
        (**self).to_json(out);
    }
}

impl<T: ToJson> ToJson for Option<T> {
    fn to_json(&self, out: &mut dyn fmt::Write) {
        match self {
            Some(value) => value.to_json(out),
            None => push_str(out, "null"),
        }
    }
}

impl<T: ToJson> ToJson for [T] {
    fn to_json(&self, out: &mut dyn fmt::Write) {
        Each(|| self.iter()).to_json(out);
    }
}

impl<T: ToJson> ToJson for VecDeque<T> {
    fn to_json(&self, out: &mut dyn fmt::Write) {
        Each(|| self.iter()).to_json(out);
    }
}

impl ToJson for TestRecord {
    fn to_json(&self, out: &mut dyn fmt::Write) {
        Object::new(out)
            .field("test", self.test)
            .field("pass", &self.pass)
            .field("cpus", &self.cpus)
            .field("took_ms", &Millis(self.took))
            .field("bytes", &self.work_bytes)
            .field("rate", &self.rate())
            .field("faults", &self.faults)
            .finish();
    }
}

impl ToJson for PassRecord {
    fn to_json(&self, out: &mut dyn fmt::Write) {
        Object::new(out)
            .field("pass", &self.pass)
            .field("took_ms", &Millis(self.took))
            .field("faults", &self.faults)
            .finish();
    }
}

/// Address, pattern, read value and the bits that differ.
fn fault_fields(o: &mut Object<'_>, fault: &Fault) {
    o.field("addr", &Hex(fault.addr.get() as u64))
        .field("expected", &Hex(fault.expected))
        .field("actual", &Hex(fault.actual))
        .field("flipped", &Hex(fault.expected ^ fault.actual));
}

impl ToJson for FaultRecord {
    fn to_json(&self, out: &mut dyn fmt::Write) {
        let mut o = Object::new(out);
        o.field("pass", &self.pass).field("test", self.test).field("cpu", &self.cpu);
        fault_fields(&mut o, &self.fault);
        o.finish();
    }
}

impl ToJson for CpuInfo {
    fn to_json(&self, out: &mut dyn fmt::Write) {
        Object::new(out)
            .field("number", &self.number)
            .field("package", &self.package)
            .field("core", &self.core)
            .field("thread", &self.thread)
            .field("bsp", &self.bsp)
            .finish();
    }
}

impl ToJson for Classification {
    fn to_json(&self, out: &mut dyn fmt::Write) {
        let (kind, stuck) = match *self {
            Self::Clean => ("clean", None),
            Self::StuckBits { mask } => ("stuck-bits", Some(mask)),
            Self::Coupling => ("coupling", None),
            Self::Mixed { stuck } => ("mixed", Some(stuck)),
        };
        Object::new(out).field("kind", kind).field("stuck", &stuck.map(Hex)).field("text", &Text(self)).finish();
    }
}

impl ToJson for PortStatus {
    fn to_json(&self, out: &mut dyn fmt::Write) {
        let link = &self.link;
        let (frames, services) = (&self.frames, &self.services);
        Object::new(out)
            .field("medium", &Text(link.medium))
            .field("mac", &Text(link.mac))
            .field("link_up", &link.up)
            .field("address", &link.address.map(Text))
            .field("gateway", &link.gateway.map(Text))
            .field("dns", &link.dns.map(Text))
            .field(
                "counters",
                &Section(|o: &mut Object<'_>| {
                    o.field("mdns_queries", &services.mdns_queries)
                        .field("mdns_replies", &services.mdns_replies)
                        .field("mdns_malformed", &services.mdns_malformed)
                        .field("http_responses", &services.http_responses)
                        .field("rx_frames", &frames.rx_frames)
                        .field("rx_dropped", &frames.rx_dropped)
                        .field("tx_frames", &frames.tx_frames)
                        .field("tx_dropped", &frames.tx_dropped)
                        .field("tx_errors", &frames.tx_errors)
                        .field("tx_reclaimed", &frames.tx_reclaimed);
                }),
            )
            .finish();
    }
}

impl ToJson for WifiFacts<'_> {
    fn to_json(&self, out: &mut dyn fmt::Write) {
        Object::new(out)
            .field("state", self.state)
            .field("ssid", &self.ssid)
            .field("retry_in_s", &self.retry_in_s)
            .field("final", &self.is_final)
            .finish();
    }
}

impl ToJson for WifiView {
    fn to_json(&self, out: &mut dyn fmt::Write) {
        self.read(|facts| facts.to_json(out));
    }
}

/// The `net` object: the host name, every port and the Wi-Fi station.
struct NetDoc<'a> {
    net: &'a NetStatus,
    wifi: &'a WifiView,
}

impl ToJson for NetDoc<'_> {
    fn to_json(&self, out: &mut dyn fmt::Write) {
        Object::new(out)
            .field("hostname", &Text(mdns::HOST))
            .field("ports", self.net.ports())
            .field("wifi", self.wifi)
            .finish();
    }
}

/// The run as of `now`, with the network it is served over.
pub struct StatusDoc<'a> {
    pub snapshot: &'a Snapshot<'a>,
    pub net: &'a NetStatus,
    pub wifi: &'a WifiView,
    pub now: Instant,
}

impl StatusDoc<'_> {
    fn since(&self, start: Instant) -> Millis {
        Millis(self.now - start)
    }
}

impl ToJson for StatusDoc<'_> {
    fn to_json(&self, out: &mut dyn fmt::Write) {
        let status: &SweepStatus = &self.snapshot.status;
        let history = self.snapshot.history;
        let preset: &str = status.preset.into();
        let mode: &str = status.mode.into();
        let (index, count) = status.position;
        Object::new(out)
            .field("version", env!("CARGO_PKG_VERSION"))
            .field("uptime_ms", &Millis(self.now.since_epoch()))
            .field("preset", preset)
            .field("paused", &status.paused)
            .field("elapsed_ms", &history.run_started.map(|start| self.since(start)))
            .field("bytes", &status.bytes)
            .field(
                "coverage",
                &Section(|o: &mut Object<'_>| {
                    o.field("tested", &status.coverage.tested).field("free", &status.coverage.free);
                }),
            )
            .field("tier", status.tier)
            .field("mode", mode)
            .field("cpus", &status.cpus)
            .field(
                "pass",
                &Section(|o: &mut Object<'_>| {
                    o.field("number", &status.passes)
                        .field("percent", &status.percent)
                        .field("elapsed_ms", &history.pass_started.map(|start| self.since(start)));
                }),
            )
            .field(
                "test",
                &Section(|o: &mut Object<'_>| {
                    o.field("name", status.test)
                        .field("index", &index)
                        .field("count", &count)
                        .field("percent", &status.test_percent);
                }),
            )
            .field("goal", &status.outlook.goal)
            .field(
                "progress",
                &ProgressDoc {
                    outlook: &status.outlook,
                    elapsed: history.run_started.map_or(Duration::ZERO, |start| self.now - start),
                    percent: status.percent,
                },
            )
            .field(
                "faults",
                &Section(|o: &mut Object<'_>| {
                    let first = status.first_fault.map(|first| {
                        Section(move |o: &mut Object<'_>| {
                            o.field("test", first.test).field("cpu", &first.cpu);
                            fault_fields(o, &first.fault);
                        })
                    });
                    o.field("total", &status.faults)
                        .field("analysis", &status.analysis)
                        .field("first", &first)
                        .field("log", self.snapshot.faults);
                }),
            )
            .field("tests", &Each(|| history.latest_tests()))
            .field("passes", &history.passes)
            .field("processors", history.cpus.as_slice())
            .field("net", &NetDoc { net: self.net, wifi: self.wifi })
            .finish();
    }
}

/// An object whose members a closure writes.
struct Section<F>(F);

impl<F: Fn(&mut Object<'_>)> ToJson for Section<F> {
    fn to_json(&self, out: &mut dyn fmt::Write) {
        let mut o = Object::new(out);
        (self.0)(&mut o);
        o.finish();
    }
}

#[cfg(test)]
mod tests {
    use alloc::rc::Rc;
    use alloc::string::String;
    use alloc::vec;
    use core::num::NonZeroU64;

    use assert2::check;
    use ferrite_mem::CpuAddr;
    use ferrite_mem::suite::{Mode, Preset};
    use ferrite_net::{Link, MacAddr, Medium, PortStats, ServiceCounters};
    use smoltcp::wire::{Ipv4Address, Ipv4Cidr};

    use super::*;
    use crate::report::History;

    fn at(ms: u64) -> Instant {
        Instant::from_epoch(Duration::from_millis(ms))
    }

    fn fault() -> Fault {
        Fault { addr: CpuAddr::of(core::ptr::without_provenance::<u8>(0x4000_1008)), expected: !0, actual: !0 ^ 0x10 }
    }

    #[test]
    fn strings_are_escaped() {
        let mut out = String::new();
        Text("a\"b\\c\nd\u{1}\u{e9}").to_json(&mut out);
        check!(out == "\"a\\\"b\\\\c\\nd\\u0001\u{e9}\"");
    }

    #[test]
    fn status_document_shape() {
        let mut history = History::default();
        history.run_started = Some(at(1_000));
        history.pass_started = Some(at(4_000));
        history.tests.push_back(TestRecord {
            pass: 0,
            test: "own-address",
            cpus: 1,
            took: Duration::from_millis(1_500),
            work_bytes: 3 << 30,
            faults: 0,
        });
        history.passes.push_back(PassRecord { pass: 0, took: Duration::from_millis(3_000), faults: 1 });
        history.cpus = vec![CpuInfo { number: 0, package: 0, core: 0, thread: 0, bsp: true }];
        let faults = VecDeque::from([FaultRecord { fault: fault(), test: "moving-inversions", cpu: 2, pass: 0 }]);
        let status = SweepStatus {
            bytes: 1 << 30,
            test: "moving-inversions",
            position: (2, 9),
            test_percent: 40,
            percent: 15,
            passes: 1,
            tier: "avx2",
            mode: Mode::Split,
            cpus: 4,
            faults: 1,
            first_fault: Some(FaultRecord { fault: fault(), test: "moving-inversions", cpu: 2, pass: 1 }),
            analysis: Classification::StuckBits { mask: 0x10 },
            preset: Preset::Thorough,
            paused: true,
            coverage: crate::sweep::Coverage::NONE,
            outlook: Outlook {
                goal: Goal { passes: NonZeroU64::new(3), time: Some(Duration::from_hours(2)), stop_on_fault: true },
                settled: 1,
                rate: Some(8_000_000_000),
                position: Position::Wait { nth: 2, waits: 4, left: Duration::from_secs(192) },
                test_left: Some(Duration::from_secs(312)),
                test_permille: Some(450),
                pass: Some(Eta { remaining: Duration::from_mins(10), confidence: Confidence::Estimated }),
                pass_permille: Some(150),
                cycle: Some(Duration::from_mins(40)),
                cycle_left: Some(Eta { remaining: Duration::from_mins(25), confidence: Confidence::Estimated }),
                known: (11, 11),
                ..Outlook::NONE
            },
        };
        let snapshot = Snapshot { status, history: &history, faults: &faults };
        let mut net = NetStatus::default();
        net.push(PortStatus {
            link: Link {
                medium: Medium::Ethernet,
                up: true,
                mac: MacAddr::new([0x52, 0x54, 0, 0x12, 0x34, 0x56]),
                address: Some(Ipv4Cidr::new(Ipv4Address::new(10, 0, 2, 15), 24)),
                gateway: Some(Ipv4Address::new(10, 0, 2, 2)),
                dns: None,
            },
            frames: PortStats { rx_frames: 5, ..PortStats::default() },
            services: ServiceCounters { http_responses: 7, ..ServiceCounters::default() },
        });
        let mut out = String::new();
        let wifi = WifiView::absent("not in this build");
        StatusDoc { snapshot: &snapshot, net: &net, wifi: &wifi, now: at(6_500) }.to_json(&mut out);
        check!(
            out == concat!(
                r#"{"version":""#,
                env!("CARGO_PKG_VERSION"),
                r#"","uptime_ms":6500,"preset":"thorough","paused":true,"elapsed_ms":5500,"bytes":1073741824,"#,
                r#""coverage":{"tested":0,"free":0},"#,
                r#""tier":"avx2","mode":"split","cpus":4,"#,
                r#""pass":{"number":1,"percent":15,"elapsed_ms":2500},"#,
                r#""test":{"name":"moving-inversions","index":2,"count":9,"percent":40},"#,
                r#""goal":{"passes":3,"time_s":7200,"stop_on_fault":true,"text":"3 passes, 2h, first fault"},"#,
                r#""progress":{"settled":1,"reserve":false,"rate":8000000000,"#,
                r#""position":{"kind":"wait","nth":2,"waits":4,"left_s":192},"#,
                r#""test":{"left_s":312,"permille":450},"pass":{"left_s":600,"confidence":"estimated"},"#,
                r#""pass_permille":150,"cycle_s":2400,"cycle_left":{"left_s":1500,"confidence":"estimated"},"#,
                r#""overall":{"elapsed_ms":5500,"permille":458,"left":{"left_s":3900,"confidence":"estimated"}},"#,
                r#""timings":{"known":11,"of":11}},"#,
                r#""faults":{"total":1,"analysis":{"kind":"stuck-bits","stuck":"0x10","text":"stuck bits 0x10"},"#,
                r#""first":{"test":"moving-inversions","cpu":2,"addr":"0x40001008","expected":"0xffffffffffffffff","#,
                r#""actual":"0xffffffffffffffef","flipped":"0x10"},"#,
                r#""log":[{"pass":0,"test":"moving-inversions","cpu":2,"addr":"0x40001008","#,
                r#""expected":"0xffffffffffffffff","actual":"0xffffffffffffffef","flipped":"0x10"}]},"#,
                r#""tests":[{"test":"own-address","pass":0,"cpus":1,"took_ms":1500,"bytes":3221225472,"#,
                r#""rate":2147483648,"faults":0}],"#,
                r#""passes":[{"pass":0,"took_ms":3000,"faults":1}],"#,
                r#""processors":[{"number":0,"package":0,"core":0,"thread":0,"bsp":true}],"#,
                r#""net":{"hostname":"ferrite.local","ports":[{"medium":"ethernet","mac":"52:54:00:12:34:56","#,
                r#""link_up":true,"address":"10.0.2.15/24","gateway":"10.0.2.2","dns":null,"#,
                r#""counters":{"mdns_queries":0,"mdns_replies":0,"mdns_malformed":0,"http_responses":7,"#,
                r#""rx_frames":5,"rx_dropped":0,"tx_frames":0,"tx_dropped":0,"tx_errors":0,"tx_reclaimed":0}}],"#,
                r#""wifi":{"state":"not in this build","ssid":null,"retry_in_s":null,"final":true}}}"#
            )
        );
    }

    #[test]
    fn the_wifi_member_follows_the_station_state() {
        use ferrite_wifi::{Failure, Shared, StationState};

        let shared = Shared::new();
        let view = WifiView::new(Rc::clone(&shared));
        let written = |view: &WifiView| {
            let mut out = String::new();
            view.to_json(&mut out);
            out
        };
        check!(written(&view) == r#"{"state":"starting","ssid":null,"retry_in_s":null,"final":false}"#);

        shared.set_state(StationState::Connecting { ssid: "ho\"me".into() });
        check!(written(&view) == r#"{"state":"connecting","ssid":"ho\"me","retry_in_s":null,"final":false}"#);

        shared.set_state(StationState::Failed { reason: Failure::Lost, retry_in: Some(Duration::from_secs(4)) });
        check!(written(&view) == r#"{"state":"the link was lost","ssid":null,"retry_in_s":4,"final":false}"#);
        let retrying = view.key();

        shared.set_state(StationState::Failed { reason: Failure::Driver, retry_in: None });
        check!(
            written(&view)
                == r#"{"state":"the card's driver is not running","ssid":null,"retry_in_s":null,"final":true}"#
        );
        check!(view.key() != retrying, "a change of state moves the key, so the document is rendered again");
    }
}
