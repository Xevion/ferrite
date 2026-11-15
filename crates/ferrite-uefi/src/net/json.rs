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
use ferrite_mem::eta::{BasisPoints, Eta, Position};

use ferrite_net::http::JsonEscape;
use ferrite_net::mdns;
use ferrite_net::{NetStatus, PortStatus};

use super::wifi::{WifiFacts, WifiView};
use crate::goal::Goal;
use crate::hardware::{Hardware, Layout};
use crate::pace::{Delta, Outlook, Steadiness, TestStat};
use crate::report::{CpuInfo, FaultRecord, PassRecord, Snapshot, TestRecord};
use crate::smbios::{MegaTransfers, Memory, MemoryDevice, Size};
use crate::sweep::{MapView, SweepStatus};
use crate::wall::Wall;

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

/// A time left: seconds, what the figure rests on, and when it ends in Unix milliseconds, which is
/// absent until the clock has a source.
fn eta_fields(o: &mut Object<'_>, eta: Eta, ends_unix_ms: Option<u64>) {
    o.field("left_s", &Secs(eta.remaining))
        .field("confidence", <&str>::from(eta.confidence))
        .field("ends_unix_ms", &ends_unix_ms);
}

/// The clock: the time in Unix milliseconds, which the page shows in its own zone, and whether the
/// network has corrected it.
struct ClockDoc<'a> {
    wall: &'a Wall,
    now: Instant,
}

impl ToJson for ClockDoc<'_> {
    fn to_json(&self, out: &mut dyn fmt::Write) {
        Object::new(out)
            .field("unix_ms", &self.wall.now(self.now).map(ferrite_hal::UnixTime::as_millis))
            .field("synced", &self.wall.is_synced())
            .finish();
    }
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

/// A fraction as its raw hundredths of a percent, 0 to 10000.
impl ToJson for BasisPoints {
    fn to_json(&self, out: &mut dyn fmt::Write) {
        self.get().to_json(out);
    }
}

/// How the run is going as time, with the run measured against its goal as of `elapsed`.
struct ProgressDoc<'a> {
    outlook: &'a Outlook,
    elapsed: Duration,
    wall: &'a Wall,
    now: Instant,
}

impl ToJson for ProgressDoc<'_> {
    fn to_json(&self, out: &mut dyn fmt::Write) {
        let o = self.outlook;
        let overall = o.overall(self.elapsed);
        let (wall, now) = (self.wall, self.now);
        let eta = |e: Option<Eta>| {
            e.map(|e| {
                let ends = wall.ends_unix_ms(now, e.remaining);
                Section(move |o: &mut Object<'_>| eta_fields(o, e, ends))
            })
        };
        Object::new(out)
            .field("settled", &o.settled)
            .field("reserve", &o.reserve)
            .field("rate", &o.rate)
            .field("position", &o.position)
            .field(
                "test",
                &Section(|s: &mut Object<'_>| {
                    s.field("left_s", &o.test_left.map(Secs))
                        .field("basis_points", &o.test_progress)
                        .field("avg_rate", &o.test_avg)
                        .field("delta_percent", &o.test_delta.map(Delta::percent));
                }),
            )
            .field("pass", &eta(o.pass))
            .field("pass_basis_points", &o.pass_progress)
            .field("cycle_s", &o.cycle.map(Secs))
            .field("cycle_left", &eta(o.cycle_left))
            .field(
                "overall",
                &Section(|s: &mut Object<'_>| {
                    s.field("elapsed_ms", &Millis(overall.elapsed))
                        .field("basis_points", &overall.fill)
                        .field("left", &eta(overall.left));
                }),
            )
            .field(
                "timings",
                &Section(|s: &mut Object<'_>| {
                    s.field("known", &o.known.0).field("of", &o.known.1);
                }),
            )
            .field("pace_percent", &o.pace_percent)
            .field("pass_median_ms", &o.pass_median.map(Millis))
            .finish();
    }
}

/// The memory map: `top`, the bytes from address 0 that the axis covers, 0 when there is no map;
/// `cell_bytes`, the bytes in one cell, rounded down; and `cells`, a string of one character for each
/// cell from address 0 up, as [`CellState::code`](crate::memmap::CellState::code) says: `n` no RAM,
/// `f` RAM the sweep does not hold (firmware), `p` the sweep's, pending, `t` the sweep's, tested,
/// `h` holding for a bit-fade wait, `a` a core working in it, `x` a fault seen in it.
impl ToJson for MapView {
    fn to_json(&self, out: &mut dyn fmt::Write) {
        push(out, '{');
        let _ = write!(out, r#""top":{},"cell_bytes":{},"cells":""#, self.top, self.cell_bytes());
        self.codes().for_each(|code| push(out, code));
        push_str(out, "\"}");
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

json_integer!(i16, u8, u16, u32, u64, u128, usize);

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

/// A finished test with what the pass measured of it.
struct TestDoc<'a> {
    record: &'a TestRecord,
    stat: Option<&'a TestStat>,
}

impl ToJson for TestDoc<'_> {
    fn to_json(&self, out: &mut dyn fmt::Write) {
        let (record, stat) = (self.record, self.stat);
        let steadiness = stat.and_then(|st| st.steadiness);
        Object::new(out)
            .field("test", record.test)
            .field("pass", &record.pass)
            .field("cpus", &record.cpus)
            .field("took_ms", &Millis(record.took))
            .field("bytes", &record.work_bytes)
            .field("rate", &record.rate())
            .field("work_rate", &stat.and_then(|st| st.rate))
            .field("delta_percent", &stat.and_then(|st| st.delta).map(Delta::percent))
            .field("median_rate", &steadiness.map(|st| st.median))
            .field("spread_basis_points", &steadiness.map(|st| st.spread))
            .field("unsteady", &steadiness.map(Steadiness::unsteady))
            .field("faults", &record.faults)
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

/// One memory slot: what the table says of it and, when it has one, the range type 20 maps to it.
struct ModuleDoc<'a> {
    device: &'a MemoryDevice,
    memory: &'a Memory,
}

impl ToJson for ModuleDoc<'_> {
    fn to_json(&self, out: &mut dyn fmt::Write) {
        let d = self.device;
        let size = match d.size {
            Size::Empty => Some(0),
            Size::Unknown => None,
            Size::Bytes(bytes) => Some(bytes.get()),
        };
        let range = self.memory.range_of(d.handle).map(|r| {
            Section(move |o: &mut Object<'_>| {
                o.field("start", &Hex(r.extent.start().get())).field("bytes", &r.extent.bytes());
            })
        });
        Object::new(out)
            .field("locator", &d.locator.as_deref())
            .field("bank", &d.bank.as_deref())
            .field("size_bytes", &size)
            .field("type", &Text(d.kind))
            .field("form_factor", &Text(d.form_factor))
            .field("speed_mts", &d.speed.map(MegaTransfers::get))
            .field("configured_mts", &d.configured_speed.map(MegaTransfers::get))
            .field("manufacturer", &d.manufacturer.as_deref())
            .field("part", &d.part_number.as_deref())
            .field("ranks", &d.ranks)
            .field("ecc", &d.has_check_bits())
            .field("range", &range)
            .finish();
    }
}

impl ToJson for Layout {
    fn to_json(&self, out: &mut dyn fmt::Write) {
        let mut o = Object::new(out);
        match self {
            Self::Unknown => o.field("kind", "unknown"),
            Self::Single => o.field("kind", "single"),
            Self::Interleaved { ways } => o.field("kind", "interleaved").field("ways", ways),
            Self::Bands(bands) => o.field("kind", "bands").field(
                "bands",
                &Each(|| {
                    bands.iter().map(|b| {
                        Section(move |o: &mut Object<'_>| {
                            o.field("label", b.label.as_str())
                                .field("start", &Hex(b.extent.start().get()))
                                .field("bytes", &b.extent.bytes());
                        })
                    })
                }),
            ),
        };
        o.finish();
    }
}

/// The `machine` object: the CPU, its clock and temperature now (null when they cannot be read), the
/// platform's ECC claim and the memory modules. The clock is in tens of MHz and the temperature in
/// whole degrees, as the screen shows them.
struct MachineDoc<'a> {
    hardware: &'a Hardware,
}

impl ToJson for MachineDoc<'_> {
    fn to_json(&self, out: &mut dyn fmt::Write) {
        let (facts, live) = (&self.hardware.facts, self.hardware.vitals());
        let cpu = &facts.cpu;
        let source = live.source.map(<&str>::from);
        Object::new(out)
            .field(
                "cpu",
                &Section(|o: &mut Object<'_>| {
                    o.field("brand", cpu.brand.as_str())
                        .field("short", facts.cpu_short.as_str())
                        .field("vendor", &Text(cpu.vendor))
                        .field("family", &cpu.signature.family)
                        .field("model", &cpu.signature.model)
                        .field("stepping", &cpu.signature.stepping);
                }),
            )
            .field("clock_mhz", &live.mhz.map(|mhz| mhz.0))
            .field("clock_measured", &live.mhz.map(|_| live.measured))
            .field("temp_c", &live.celsius)
            .field("temp_source", &source)
            .field("ecc", &facts.ecc_claim().map(Text))
            .field("corrects_errors", &facts.corrects_errors())
            .field("modules_text", facts.modules.as_str())
            .field("layout", &facts.layout)
            .field("layout_caption", &facts.caption.as_deref())
            .field(
                "modules",
                &Each(|| facts.memory.devices.iter().map(|device| ModuleDoc { device, memory: &facts.memory })),
            )
            .finish();
    }
}

/// The run as of `now`, with the network it is served over.
pub struct StatusDoc<'a> {
    pub snapshot: &'a Snapshot<'a>,
    pub net: &'a NetStatus,
    pub wifi: &'a WifiView,
    pub now: Instant,
    pub wall: &'a Wall,
    pub hardware: &'a Hardware,
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
            .field("clock", &ClockDoc { wall: self.wall, now: self.now })
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
            .field("map", &status.map)
            .field("tier", status.tier)
            .field("bandwidth", &status.outlook.bandwidth)
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
                    wall: self.wall,
                    now: self.now,
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
            .field(
                "tests",
                &Each(|| {
                    history
                        .latest_tests()
                        .map(|record| TestDoc { record, stat: self.snapshot.stats.of(record.test, record.pass) })
                }),
            )
            .field("passes", &history.passes)
            .field("processors", history.cpus.as_slice())
            .field("machine", &MachineDoc { hardware: self.hardware })
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
    use ferrite_config::{ClockFormat, zone_named};
    use ferrite_efi::sensors::{Frequency, Identity, Reading, TempSource, Temperature};
    use ferrite_hal::{PhysAddr, UnixTime, WallClock};
    use ferrite_mem::CpuAddr;
    use ferrite_mem::eta::Confidence;
    use ferrite_mem::histogram::Histogram;
    use ferrite_mem::suite::{Mode, Preset};
    use ferrite_net::{Link, MacAddr, Medium, PortStats, ServiceCounters};
    use ferrite_power::cpuid::{Signature, Vendor};
    use ferrite_power::units::Mhz;
    use smoltcp::wire::{Ipv4Address, Ipv4Cidr};

    use super::*;
    use crate::hardware::{Facts, Vitals};
    use crate::memmap::{CellState, Extent};
    use crate::pace::TestStats;
    use crate::report::History;
    use crate::smbios::{DeviceRange, ErrorCorrection, FormFactor, Handle, MemoryArray, MemoryKind};

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

    /// The stats of one finished test: `own-address` of pass 0, with six samples at 2.4 GB/s.
    fn finished_own_address() -> (TestStats, Steadiness) {
        let mut samples = Histogram::new();
        for _ in 0..6 {
            samples.record(2_400_000_000);
        }
        let steadiness = Steadiness::of(&samples).unwrap();
        let mut finished = TestStats::default();
        finished.reset(9);
        finished.set(
            1,
            TestStat {
                test: "own-address",
                pass: 0,
                rate: Some(2_300_000_000),
                delta: Delta::between(2_300_000_000, 2_500_000_000),
                steadiness: Some(steadiness),
            },
        );
        (finished, steadiness)
    }

    /// A run waiting in the second of bit fade's four waits, towards three passes in two hours.
    fn live_outlook() -> Outlook {
        Outlook {
            goal: Goal { passes: NonZeroU64::new(3), time: Some(Duration::from_hours(2)), stop_on_fault: true },
            settled: 1,
            rate: Some(8_000_000_000),
            test_avg: Some(7_900_000_000),
            test_delta: Delta::between(7_900_000_000, 8_400_000_000),
            position: Position::Wait { nth: 2, waits: 4, left: Duration::from_secs(192) },
            test_left: Some(Duration::from_secs(312)),
            test_progress: BasisPoints::new(4512).unwrap(),
            pass: Some(Eta { remaining: Duration::from_mins(10), confidence: Confidence::Estimated }),
            pass_progress: BasisPoints::new(1537).unwrap(),
            cycle: Some(Duration::from_mins(40)),
            cycle_left: Some(Eta { remaining: Duration::from_mins(25), confidence: Confidence::Estimated }),
            cycle_progress: BasisPoints::new(3750),
            known: (11, 11),
            pace_percent: Some(98),
            bandwidth: Some(18_400_000_000),
            pass_median: Some(Duration::from_secs(128)),
            ..Outlook::NONE
        }
    }

    /// The first eight cells are tested, tested, active, pending, a fault, the firmware's, no RAM and
    /// holding; the rest are pending.
    fn sample_map() -> MapView {
        let mut map = MapView { top: 18 << 30, cells: [CellState::Pending; crate::dash::CELLS] };
        map.cells[..3].fill(CellState::Done);
        map.cells[2..8].copy_from_slice(&[
            CellState::Active,
            CellState::Pending,
            CellState::Fault,
            CellState::Firmware,
            CellState::NoRam,
            CellState::Holding,
        ]);
        map
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
            outlook: live_outlook(),
            map: sample_map(),
        };
        let (finished, steadiness) = finished_own_address();
        let snapshot = Snapshot { status, history: &history, faults: &faults, stats: &finished };
        let mut net = NetStatus::default();
        net.push(PortStatus {
            link: Link {
                medium: Medium::Ethernet,
                up: true,
                mac: MacAddr::new([0x52, 0x54, 0, 0x12, 0x34, 0x56]),
                address: Some(Ipv4Cidr::new(Ipv4Address::new(10, 0, 2, 15), 24)),
                gateway: Some(Ipv4Address::new(10, 0, 2, 2)),
                dns: None,
                ..Link::default()
            },
            frames: PortStats { rx_frames: 5, ..PortStats::default() },
            services: ServiceCounters { http_responses: 7, ..ServiceCounters::default() },
        });
        let mut out = String::new();
        let wifi = WifiView::absent("not in this build");
        // The RTC said 1794646670 s at the start, so 6.5 s in the time is 1794646676500 ms.
        let seed = WallClock::from_rtc(at(0), UnixTime::from_epoch(Duration::from_secs(1_794_646_670)));
        let wall = Wall::new(Some(seed), zone_named("UTC").unwrap(), ClockFormat::Twelve);
        let hardware = Hardware::default();
        StatusDoc { snapshot: &snapshot, net: &net, wifi: &wifi, now: at(6_500), wall: &wall, hardware: &hardware }
            .to_json(&mut out);
        let expected = concat!(
            r#"{"version":""#,
            env!("CARGO_PKG_VERSION"),
            r#"","uptime_ms":6500,"clock":{"unix_ms":1794646676500,"synced":false},"preset":"thorough","paused":true,"elapsed_ms":5500,"bytes":1073741824,"#,
            r#""coverage":{"tested":0,"free":0},"#,
            r#""map":{"top":19327352832,"cell_bytes":150994944,"cells":"ttapxfnh"#,
            "@PENDING@",
            r#""},"#,
            r#""tier":"avx2","bandwidth":18400000000,"mode":"split","cpus":4,"#,
            r#""pass":{"number":1,"percent":15,"elapsed_ms":2500},"#,
            r#""test":{"name":"moving-inversions","index":2,"count":9,"percent":40},"#,
            r#""goal":{"passes":3,"time_s":7200,"stop_on_fault":true,"text":"3 passes, 2h, first fault"},"#,
            r#""progress":{"settled":1,"reserve":false,"rate":8000000000,"#,
            r#""position":{"kind":"wait","nth":2,"waits":4,"left_s":192},"#,
            r#""test":{"left_s":312,"basis_points":4512,"avg_rate":7900000000,"delta_percent":-6},"#,
            r#""pass":{"left_s":600,"confidence":"estimated","ends_unix_ms":1794647276500},"pass_basis_points":1537,"#,
            r#""cycle_s":2400,"cycle_left":{"left_s":1500,"confidence":"estimated","ends_unix_ms":1794648176500},"#,
            r#""overall":{"elapsed_ms":5500,"basis_points":4583,"#,
            r#""left":{"left_s":3900,"confidence":"estimated","ends_unix_ms":1794650576500}},"#,
            r#""timings":{"known":11,"of":11},"pace_percent":98,"pass_median_ms":128000},"#,
            r#""faults":{"total":1,"analysis":{"kind":"stuck-bits","stuck":"0x10","text":"stuck bits 0x10"},"#,
            r#""first":{"test":"moving-inversions","cpu":2,"addr":"0x40001008","expected":"0xffffffffffffffff","#,
            r#""actual":"0xffffffffffffffef","flipped":"0x10"},"#,
            r#""log":[{"pass":0,"test":"moving-inversions","cpu":2,"addr":"0x40001008","#,
            r#""expected":"0xffffffffffffffff","actual":"0xffffffffffffffef","flipped":"0x10"}]},"#,
            r#""tests":[{"test":"own-address","pass":0,"cpus":1,"took_ms":1500,"bytes":3221225472,"#,
            r#""rate":2147483648,"work_rate":2300000000,"delta_percent":-8,"median_rate":"#,
            "@MEDIAN@",
            r#","spread_basis_points":0,"unsteady":false,"faults":0}],"#,
            r#""passes":[{"pass":0,"took_ms":3000,"faults":1}],"#,
            r#""processors":[{"number":0,"package":0,"core":0,"thread":0,"bsp":true}],"#,
            r#""machine":{"cpu":{"brand":"","short":"","vendor":"","family":0,"model":0,"stepping":0},"#,
            r#""clock_mhz":null,"clock_measured":null,"temp_c":null,"temp_source":null,"ecc":null,"#,
            r#""corrects_errors":null,"modules_text":"","layout":{"kind":"unknown"},"layout_caption":null,"modules":[]},"#,
            r#""net":{"hostname":"ferrite.local","ports":[{"medium":"ethernet","mac":"52:54:00:12:34:56","#,
            r#""link_up":true,"address":"10.0.2.15/24","gateway":"10.0.2.2","dns":null,"#,
            r#""counters":{"mdns_queries":0,"mdns_replies":0,"mdns_malformed":0,"http_responses":7,"#,
            r#""rx_frames":5,"rx_dropped":0,"tx_frames":0,"tx_dropped":0,"tx_errors":0,"tx_reclaimed":0}}],"#,
            r#""wifi":{"state":"not in this build","ssid":null,"retry_in_s":null,"final":true}}}"#
        )
        .replace("@MEDIAN@", &std::format!("{}", steadiness.median))
        .replace("@PENDING@", &"p".repeat(crate::dash::CELLS - 8));
        check!(out == expected);
    }

    #[test]
    fn the_machine_object_describes_the_cpu_the_modules_and_the_sensors() {
        let cpu = Identity {
            vendor: Vendor::Intel,
            signature: Signature { family: 6, model: 0x8e, stepping: 10 },
            brand: "Intel(R) Core(TM) i7-8650U CPU @ 1.90GHz".into(),
        };
        let module = |handle: u16, locator: &str| MemoryDevice {
            handle: Handle(handle),
            array: Handle(1),
            locator: Some(locator.into()),
            bank: None,
            size: Size::Bytes(NonZeroU64::new(8 << 30).unwrap()),
            kind: MemoryKind::Lpddr3,
            form_factor: FormFactor::Chip,
            speed: MegaTransfers::new(2133),
            configured_speed: MegaTransfers::new(2133),
            manufacturer: Some("Samsung".into()),
            part_number: Some("K4E".into()),
            ranks: Some(1),
            data_width: Some(64),
            total_width: Some(72),
        };
        let mut empty = module(3, "ChannelC");
        empty.size = Size::Empty;
        let mut memory =
            Memory { devices: vec![module(1, "ChannelA"), module(2, "ChannelB"), empty], ..Memory::default() };
        memory.arrays.push(MemoryArray {
            handle: Handle(1),
            system: true,
            correction: ErrorCorrection::SingleBitEcc,
            slots: 3,
        });
        for device in [1, 2] {
            memory.device_ranges.push(DeviceRange {
                device: Handle(device),
                array_range: Handle(9),
                extent: Extent::new(PhysAddr::new(0), 16 << 30),
                position: Some(0),
                depth: Some(1),
            });
        }
        let hardware = Hardware::new(Facts::new(cpu, memory));
        hardware.set_vitals(Vitals::of(Reading {
            frequency: Some(Frequency { mhz: Mhz(3_396), measured: true }),
            temperature: Some(Temperature { celsius: 58, source: TempSource::IntelPackage }),
        }));
        let mut out = String::new();
        MachineDoc { hardware: &hardware }.to_json(&mut out);
        let expected = concat!(
            r#"{"cpu":{"brand":"Intel(R) Core(TM) i7-8650U CPU @ 1.90GHz","short":"Core i7-8650U","#,
            r#""vendor":"GenuineIntel","family":6,"model":142,"stepping":10},"#,
            r#""clock_mhz":3400,"clock_measured":true,"temp_c":58,"temp_source":"intel-package","#,
            r#""ecc":"single-bit ECC","corrects_errors":true,"modules_text":"2 x 8 GiB LPDDR3-2133, 1 empty","#,
            r#""layout":{"kind":"interleaved","ways":2},"layout_caption":"2 channels interleaved","#,
            r#""modules":["#,
            r#"{"locator":"ChannelA","bank":null,"size_bytes":8589934592,"type":"LPDDR3","form_factor":"chip","#,
            r#""speed_mts":2133,"configured_mts":2133,"manufacturer":"Samsung","part":"K4E","ranks":1,"ecc":true,"#,
            r#""range":{"start":"0x0","bytes":17179869184}},"#,
            r#"{"locator":"ChannelB","bank":null,"size_bytes":8589934592,"type":"LPDDR3","form_factor":"chip","#,
            r#""speed_mts":2133,"configured_mts":2133,"manufacturer":"Samsung","part":"K4E","ranks":1,"ecc":true,"#,
            r#""range":{"start":"0x0","bytes":17179869184}},"#,
            r#"{"locator":"ChannelC","bank":null,"size_bytes":0,"type":"LPDDR3","form_factor":"chip","#,
            r#""speed_mts":2133,"configured_mts":2133,"manufacturer":"Samsung","part":"K4E","ranks":1,"ecc":true,"#,
            r#""range":null}]}"#
        );
        check!(out == expected);
    }

    #[test]
    fn a_clock_with_no_source_gives_no_times() {
        let wall = Wall::new(None, zone_named("UTC").unwrap(), ClockFormat::Twelve);
        let mut out = String::new();
        ClockDoc { wall: &wall, now: at(1_000) }.to_json(&mut out);
        check!(out == r#"{"unix_ms":null,"synced":false}"#);

        let outlook = live_outlook();
        let mut out = String::new();
        ProgressDoc { outlook: &outlook, elapsed: Duration::from_secs(5), wall: &wall, now: at(1_000) }
            .to_json(&mut out);
        check!(out.contains(r#""pass":{"left_s":600,"confidence":"estimated","ends_unix_ms":null}"#), "{out}");
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
