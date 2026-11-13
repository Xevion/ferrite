//! A small typed JSON writer and the document `/api/status` and the event stream send.
//!
//! Each type that appears in the document says how it is written through [`ToJson`]. Numbers are
//! integers only: 64-bit patterns and addresses go out as hex strings, since a JavaScript number
//! keeps only 53 bits.

use alloc::collections::VecDeque;
use alloc::string::String;
use core::fmt::{self, Write as _};
use core::time::Duration;

use ferrite_hal::Instant;
use ferrite_mem::Fault;
use ferrite_mem::analysis::Classification;

use crate::mdns;
use crate::net::{Counters, NetStatus};
use crate::report::{CpuInfo, FaultRecord, PassRecord, Snapshot, TestRecord};
use crate::sweep::SweepStatus;

pub trait ToJson {
    fn to_json(&self, out: &mut String);
}

/// Writes the members of one object.
pub struct Object<'a> {
    out: &'a mut String,
    empty: bool,
}

impl<'a> Object<'a> {
    pub fn new(out: &'a mut String) -> Self {
        out.push('{');
        Self { out, empty: true }
    }

    pub fn field(&mut self, key: &str, value: &(impl ToJson + ?Sized)) -> &mut Self {
        if !self.empty {
            self.out.push(',');
        }
        self.empty = false;
        key.to_json(self.out);
        self.out.push(':');
        value.to_json(self.out);
        self
    }

    pub fn finish(&mut self) {
        self.out.push('}');
    }
}

/// Escapes what is written through it as the inside of a JSON string.
struct Escape<'a>(&'a mut String);

impl fmt::Write for Escape<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.chars() {
            match c {
                '"' => self.0.push_str("\\\""),
                '\\' => self.0.push_str("\\\\"),
                '\n' => self.0.push_str("\\n"),
                c if u32::from(c) < 0x20 => write!(self.0, "\\u{:04x}", u32::from(c))?,
                c => self.0.push(c),
            }
        }
        Ok(())
    }
}

/// A value written as a JSON string through its `Display`.
pub struct Text<T>(pub T);

impl<T: fmt::Display> ToJson for Text<T> {
    fn to_json(&self, out: &mut String) {
        out.push('"');
        // Writing into a String cannot fail.
        let _ = write!(Escape(out), "{}", self.0);
        out.push('"');
    }
}

/// A 64-bit value as a `0x`-prefixed hex string.
pub struct Hex(pub u64);

impl ToJson for Hex {
    fn to_json(&self, out: &mut String) {
        let _ = write!(out, "\"{:#x}\"", self.0);
    }
}

/// A duration as whole milliseconds.
pub struct Millis(pub Duration);

impl ToJson for Millis {
    fn to_json(&self, out: &mut String) {
        self.0.as_millis().to_json(out);
    }
}

/// The items of an iterator, as an array.
pub struct Each<F>(pub F);

impl<F, I> ToJson for Each<F>
where
    F: Fn() -> I,
    I: Iterator<Item: ToJson>,
{
    fn to_json(&self, out: &mut String) {
        out.push('[');
        for (i, item) in (self.0)().enumerate() {
            if i > 0 {
                out.push(',');
            }
            item.to_json(out);
        }
        out.push(']');
    }
}

macro_rules! json_integer {
    ($($t:ty),*) => {$(
        impl ToJson for $t {
            fn to_json(&self, out: &mut String) {
                let _ = write!(out, "{self}");
            }
        }
    )*};
}

json_integer!(u16, u32, u64, u128, usize);

impl ToJson for bool {
    fn to_json(&self, out: &mut String) {
        out.push_str(if *self { "true" } else { "false" });
    }
}

impl ToJson for str {
    fn to_json(&self, out: &mut String) {
        Text(self).to_json(out);
    }
}

impl<T: ToJson + ?Sized> ToJson for &T {
    fn to_json(&self, out: &mut String) {
        (**self).to_json(out);
    }
}

impl<T: ToJson> ToJson for Option<T> {
    fn to_json(&self, out: &mut String) {
        match self {
            Some(value) => value.to_json(out),
            None => out.push_str("null"),
        }
    }
}

impl<T: ToJson> ToJson for [T] {
    fn to_json(&self, out: &mut String) {
        Each(|| self.iter()).to_json(out);
    }
}

impl<T: ToJson> ToJson for VecDeque<T> {
    fn to_json(&self, out: &mut String) {
        Each(|| self.iter()).to_json(out);
    }
}

impl ToJson for TestRecord {
    fn to_json(&self, out: &mut String) {
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
    fn to_json(&self, out: &mut String) {
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
    fn to_json(&self, out: &mut String) {
        let mut o = Object::new(out);
        o.field("pass", &self.pass).field("test", self.test).field("cpu", &self.cpu);
        fault_fields(&mut o, &self.fault);
        o.finish();
    }
}

impl ToJson for CpuInfo {
    fn to_json(&self, out: &mut String) {
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
    fn to_json(&self, out: &mut String) {
        let (kind, stuck) = match *self {
            Self::Clean => ("clean", None),
            Self::StuckBits { mask } => ("stuck-bits", Some(mask)),
            Self::Coupling => ("coupling", None),
            Self::Mixed { stuck } => ("mixed", Some(stuck)),
        };
        Object::new(out).field("kind", kind).field("stuck", &stuck.map(Hex)).field("text", &Text(self)).finish();
    }
}

impl ToJson for Counters {
    fn to_json(&self, out: &mut String) {
        Object::new(out)
            .field("mdns_queries", &self.mdns_queries)
            .field("mdns_replies", &self.mdns_replies)
            .field("mdns_malformed", &self.mdns_malformed)
            .field("http_responses", &self.http_responses)
            .field("rx_frames", &self.rx_frames)
            .field("rx_errors", &self.rx_errors)
            .field("tx_errors", &self.tx_errors)
            .field("tx_stale_reclaims", &self.tx_stale_reclaims)
            .finish();
    }
}

/// The mDNS hostname, labels joined by dots.
struct Hostname;

impl fmt::Display for Hostname {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, label) in mdns::HOST.labels().iter().enumerate() {
            if i > 0 {
                f.write_char('.')?;
            }
            f.write_str(core::str::from_utf8(label.as_bytes()).map_err(|_| fmt::Error)?)?;
        }
        Ok(())
    }
}

impl ToJson for NetStatus {
    fn to_json(&self, out: &mut String) {
        let link = &self.link;
        Object::new(out)
            .field("hostname", &Text(Hostname))
            .field("mac", &Text(link.mac))
            .field("link_up", &link.up)
            .field("address", &link.address.map(Text))
            .field("gateway", &link.gateway.map(Text))
            .field("dns", &link.dns.map(Text))
            .field("counters", &self.counters)
            .finish();
    }
}

/// The run as of `now`, with the network it is served over.
pub struct StatusDoc<'a> {
    pub snapshot: &'a Snapshot<'a>,
    pub net: &'a NetStatus,
    pub now: Instant,
}

impl StatusDoc<'_> {
    fn since(&self, start: Instant) -> Millis {
        Millis(self.now - start)
    }
}

impl ToJson for StatusDoc<'_> {
    fn to_json(&self, out: &mut String) {
        let status: &SweepStatus = &self.snapshot.status;
        let history = self.snapshot.history;
        let preset: &str = status.preset.into();
        let mode: &str = status.mode.into();
        let (index, count) = status.position;
        Object::new(out)
            .field("uptime_ms", &Millis(self.now.since_epoch()))
            .field("preset", preset)
            .field("paused", &status.paused)
            .field("elapsed_ms", &history.run_started.map(|start| self.since(start)))
            .field("bytes", &status.bytes)
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
            .field("net", self.net)
            .finish();
    }
}

/// An object whose members a closure writes.
struct Section<F>(F);

impl<F: Fn(&mut Object<'_>)> ToJson for Section<F> {
    fn to_json(&self, out: &mut String) {
        let mut o = Object::new(out);
        (self.0)(&mut o);
        o.finish();
    }
}

/// `{"error": message}`.
pub fn error(message: impl fmt::Display) -> String {
    let mut out = String::new();
    Object::new(&mut out).field("error", &Text(message)).finish();
    out
}

/// `{"accepted": name}`, the reply to a control request.
pub fn accepted(name: &str) -> String {
    let mut out = String::new();
    Object::new(&mut out).field("accepted", name).finish();
    out
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use assert2::check;
    use ferrite_mem::CpuAddr;
    use ferrite_mem::suite::{Mode, Preset};
    use smoltcp::wire::{Ipv4Address, Ipv4Cidr};

    use super::*;
    use crate::net::{Link, MacAddr};
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
        };
        let snapshot = Snapshot { status, history: &history, faults: &faults };
        let net = NetStatus {
            link: Link {
                up: true,
                mac: MacAddr::new([0x52, 0x54, 0, 0x12, 0x34, 0x56]),
                address: Some(Ipv4Cidr::new(Ipv4Address::new(10, 0, 2, 15), 24)),
                gateway: Some(Ipv4Address::new(10, 0, 2, 2)),
                dns: None,
            },
            counters: Counters { http_responses: 7, ..Counters::default() },
        };
        let mut out = String::new();
        StatusDoc { snapshot: &snapshot, net: &net, now: at(6_500) }.to_json(&mut out);
        check!(
            out == concat!(
                r#"{"uptime_ms":6500,"preset":"thorough","paused":true,"elapsed_ms":5500,"bytes":1073741824,"#,
                r#""tier":"avx2","mode":"split","cpus":4,"#,
                r#""pass":{"number":1,"percent":15,"elapsed_ms":2500},"#,
                r#""test":{"name":"moving-inversions","index":2,"count":9,"percent":40},"#,
                r#""faults":{"total":1,"analysis":{"kind":"stuck-bits","stuck":"0x10","text":"stuck bits 0x10"},"#,
                r#""first":{"test":"moving-inversions","cpu":2,"addr":"0x40001008","expected":"0xffffffffffffffff","#,
                r#""actual":"0xffffffffffffffef","flipped":"0x10"},"#,
                r#""log":[{"pass":0,"test":"moving-inversions","cpu":2,"addr":"0x40001008","#,
                r#""expected":"0xffffffffffffffff","actual":"0xffffffffffffffef","flipped":"0x10"}]},"#,
                r#""tests":[{"test":"own-address","pass":0,"cpus":1,"took_ms":1500,"bytes":3221225472,"#,
                r#""rate":2147483648,"faults":0}],"#,
                r#""passes":[{"pass":0,"took_ms":3000,"faults":1}],"#,
                r#""processors":[{"number":0,"package":0,"core":0,"thread":0,"bsp":true}],"#,
                r#""net":{"hostname":"ferrite.local","mac":"52:54:00:12:34:56","link_up":true,"#,
                r#""address":"10.0.2.15/24","gateway":"10.0.2.2","dns":null,"#,
                r#""counters":{"mdns_queries":0,"mdns_replies":0,"mdns_malformed":0,"http_responses":7,"#,
                r#""rx_frames":0,"rx_errors":0,"tx_errors":0,"tx_stale_reclaims":0}}}"#
            )
        );
    }
}
