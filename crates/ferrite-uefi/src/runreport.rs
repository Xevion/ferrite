//! The end-of-run report: what the run found and how it went, as plain text a person can read later
//! on any computer.
//!
//! [`Report::render`] builds it from a [`Snapshot`] of the sweep, the machine's [`Hardware`] and the
//! [`Wall`] clock, on demand and never in the test loop. Where it is written and under what name is
//! [`store`]'s.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::{self, Write as _};
use core::time::Duration;

use ferrite_hal::Instant;

use crate::dash::format::{self, hms};
use crate::goal::Met;
use crate::hardware::{Hardware, Layout, ModuleLine};
use crate::pace::Outlook;
use crate::report::{FaultRecord, Snapshot, TestRecord};
use crate::wall::Wall;

pub mod store;

/// The version the report names.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Columns of the test table: the name, then the right-aligned figures.
const NAME_WIDTH: usize = 34;
/// Flipped bits listed for one fault before the rest are left as the mask alone.
const BITS_LISTED: usize = 8;

/// Why the report was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// Asked for, with F10, the status page or its API, while the run goes on.
    Requested,
    /// The run's goal was met and the run ended for it.
    GoalMet(Met),
    /// The run was ended by a quit key, the status page or the desktop.
    Quit,
}

impl Why {
    /// Whether the run is over.
    const fn run_ended(self) -> bool {
        !matches!(self, Self::Requested)
    }
}

impl fmt::Display for Why {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Requested => f.write_str("requested (F10 or the status page); the run was still going"),
            Self::GoalMet(met) => write!(f, "goal met: {met}"),
            Self::Quit => f.write_str("the run was quit"),
        }
    }
}

/// What the run came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail { faults: u64 },
    Incomplete,
}

impl Verdict {
    /// A fault is a failure however far the run got; without one, a pass counts only once a whole
    /// pass (its reserve rotation included, which is what `settled` counts) has finished.
    #[must_use]
    pub const fn of(faults: u64, settled: u64) -> Self {
        if faults > 0 {
            Self::Fail { faults }
        } else if settled > 0 {
            Self::Pass
        } else {
            Self::Incomplete
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pass => f.write_str("PASS (no faults)"),
            Self::Fail { faults } => write!(f, "FAIL ({faults} fault{})", format::plural(*faults)),
            Self::Incomplete => f.write_str("INCOMPLETE (no full pass finished)"),
        }
    }
}

/// Everything a report is made of.
pub struct Report<'a> {
    pub snapshot: &'a Snapshot<'a>,
    pub hardware: &'a Hardware,
    pub wall: &'a Wall,
    /// When the report is written, and the run's end when it is over.
    pub now: Instant,
    pub why: Why,
    pub version: &'a str,
}

/// Lines written into a string, which cannot fail.
struct Page<'a>(&'a mut String);

impl Page<'_> {
    fn line(&mut self, args: fmt::Arguments<'_>) {
        let _ = self.0.write_fmt(args);
        self.0.push('\n');
    }

    fn blank(&mut self) {
        self.0.push('\n');
    }

    /// A row of a section: a key and its value.
    fn row(&mut self, key: &str, value: impl fmt::Display) {
        self.line(format_args!("{key:<14}{value}"));
    }

    fn section(&mut self, title: &str) {
        self.blank();
        self.line(format_args!("{title}"));
        self.line(format_args!("{:-<1$}", "", title.len()));
    }
}

impl Report<'_> {
    #[must_use]
    pub const fn verdict(&self) -> Verdict {
        Verdict::of(self.snapshot.status.faults, self.snapshot.status.outlook.settled)
    }

    fn elapsed(&self) -> Option<Duration> {
        self.snapshot.history.run_started.map(|start| self.now - start)
    }

    /// `2026-11-14 05:23:07 America/Chicago`, with what is wrong with the clock when it has no
    /// correction from the network; the time alone, in a 24 hour clock whatever the screens use.
    fn stamp(&self, at: Instant) -> String {
        let Some(time) = self.wall.local_time(at) else { return "clock not synced".into() };
        let mut out = String::new();
        let _ = write!(
            out,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02} {}",
            time.year(),
            time.month(),
            time.day(),
            time.hour(),
            time.minute(),
            time.second(),
            self.wall.zone_name()
        );
        if !self.wall.is_synced() {
            out.push_str(" (clock not synced)");
        }
        out
    }

    /// The report as text.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(16 << 10);
        self.render_into(&mut out);
        out
    }

    /// Append the report to `out`.
    pub fn render_into(&self, out: &mut String) {
        let mut page = Page(out);
        self.header(&mut page);
        self.machine(&mut page);
        self.run(&mut page);
        self.passes(&mut page);
        self.faults(&mut page);
    }

    fn header(&self, p: &mut Page<'_>) {
        p.line(format_args!("FERRITE MEMORY TEST REPORT"));
        p.blank();
        p.row("Verdict", self.verdict());
        p.row("Version", format_args!("ferrite {}", self.version));
        p.row("Written", self.stamp(self.now));
        p.row("Reason", self.why);
    }

    fn machine(&self, p: &mut Page<'_>) {
        let facts = &self.hardware.facts;
        let machine = self.hardware.machine();
        p.section("MACHINE");
        p.row("Machine id", format_args!("{:016x} (from {})", machine.id, machine.source));
        p.row("CPU", Ascii(if facts.cpu.brand.is_empty() { "unknown" } else { &facts.cpu.brand }));
        p.row("Signature", format_args!("{} {}", facts.cpu.vendor, facts.cpu.signature));
        p.row(
            "Memory",
            if facts.modules.is_empty() { "no modules in the SMBIOS table" } else { facts.modules.as_str() },
        );
        match facts.ecc_claim() {
            Some(claim) => p.row("ECC", format_args!("{claim} (the firmware's claim, not verified)")),
            None => p.row("ECC", "the firmware makes no claim"),
        }
        if let Some(caption) = &facts.caption {
            p.row("Layout", caption);
        }
        for (i, device) in facts.memory.devices.iter().enumerate() {
            p.row(if i == 0 { "Modules" } else { "" }, ModuleLine(device));
        }
        let vitals = self.hardware.vitals();
        match vitals.mhz {
            Some(mhz) => p.row(
                "Clock",
                format_args!("{mhz} ({})", if vitals.measured { "measured" } else { "base, not measured" }),
            ),
            None => p.row("Clock", "unknown"),
        }
        match (vitals.celsius, vitals.source) {
            (Some(now), source) => {
                let peak = self.hardware.peak_celsius().unwrap_or(now);
                p.row("Temperature", format_args!("{now} C at the last reading, {peak} C the highest of the run"));
                if let Some(source) = source {
                    p.row("", format_args!("read from {source}"));
                }
            },
            (None, _) => p.row("Temperature", "unknown"),
        }
    }

    fn run(&self, p: &mut Page<'_>) {
        let status = &self.snapshot.status;
        let outlook = &status.outlook;
        p.section("RUN");
        p.row("Preset", <&str>::from(status.preset));
        p.row("Kernels", status.tier);
        p.row("Cores", Cores(&self.snapshot.history.cpus));
        p.row("Goal", GoalLine { outlook, report: self });
        let history = self.snapshot.history;
        match history.run_started {
            Some(start) => p.row("Started", self.stamp(start)),
            None => p.row("Started", "not yet"),
        }
        if self.why.run_ended() {
            p.row("Ended", self.stamp(self.now));
        } else {
            p.row("Ended", "still running");
        }
        p.row("Elapsed", hms(self.elapsed().unwrap_or(Duration::ZERO)));
        p.row("Passes", format_args!("{} (a pass counts once its reserve rotation has run)", outlook.settled));
        p.row("Memory", format_args!("{} MiB under test", status.coverage.tested >> 20));
        p.row("Coverage", status.coverage);
        if let Some(median) = outlook.pass_median {
            p.row("Median pass", format::took(median));
        }
        if let Some(cycle) = outlook.cycle {
            p.row("Predicted", format_args!("{} for a pass with its reserve rotation", format::took(cycle)));
        }
        if let Some(percent) = outlook.pace_percent {
            p.row("Against saved", format_args!("{percent}% of the time the same tests took before"));
        }
        if let Some(rate) = outlook.bandwidth {
            p.row("Bandwidth", format_args!("{} measured before the first test", format::rate(rate)));
        }
    }

    fn passes(&self, p: &mut Page<'_>) {
        let history = self.snapshot.history;
        p.section("PASSES");
        let mut numbers: Vec<u64> = history
            .passes
            .iter()
            .map(|r| r.pass)
            .chain(history.tests.iter().map(|r| r.pass))
            .chain(history.reserve.iter().map(|r| r.pass))
            .collect();
        numbers.sort_unstable();
        numbers.dedup();
        if numbers.is_empty() {
            p.line(format_args!("No test has finished yet."));
            return;
        }
        for pass in numbers {
            let tests: Vec<&TestRecord> = history.tests.iter().filter(|r| r.pass == pass).collect();
            let reserve: Vec<&TestRecord> = history.reserve.iter().filter(|r| r.pass == pass).collect();
            p.blank();
            match history.passes.iter().find(|r| r.pass == pass) {
                Some(done) if tests.is_empty() => p.line(format_args!(
                    "Pass {}: {}, {} faults (its tests are no longer kept, only the newest are)",
                    pass + 1,
                    format::took(done.took),
                    done.faults
                )),
                Some(done) => {
                    p.line(format_args!("Pass {}: {}, {} faults", pass + 1, format::took(done.took), done.faults));
                },
                None => p.line(format_args!(
                    "Pass {}: under way, {} test{} finished, {} faults so far",
                    pass + 1,
                    tests.len(),
                    format::plural(tests.len()),
                    tests.iter().map(|r| r.faults).sum::<u64>()
                )),
            }
            table(p, &tests);
            if !reserve.is_empty() {
                let took: Duration = reserve.iter().map(|r| r.took).sum();
                p.line(format_args!(
                    "  Reserve rotation after pass {}: {}, {} faults",
                    pass + 1,
                    format::took(took),
                    reserve.iter().map(|r| r.faults).sum::<u64>()
                ));
                table(p, &reserve);
            }
        }
    }

    fn faults(&self, p: &mut Page<'_>) {
        let status = &self.snapshot.status;
        let kept = self.snapshot.faults;
        p.section("FAULTS");
        p.row("Total", format_args!("{} word{} read back wrong", status.faults, format::plural(status.faults)));
        p.row("Analysis", status.analysis);
        if kept.is_empty() {
            return;
        }
        let kept_note = if u64::try_from(kept.len()).is_ok_and(|n| n < status.faults) {
            format!("the newest {} of {}", kept.len(), status.faults)
        } else {
            format!("all {}", kept.len())
        };
        p.row("Listed", format_args!("{kept_note}, oldest first"));
        for (i, record) in kept.iter().enumerate() {
            self.fault(p, i + 1, record);
        }
    }

    fn fault(&self, p: &mut Page<'_>, number: usize, record: &FaultRecord) {
        let fault = record.fault;
        let address = fault.addr.get() as u64;
        let flipped = fault.expected ^ fault.actual;
        p.blank();
        match module_of(&self.hardware.facts.layout, address) {
            Some(module) => p.line(format_args!("{number:>3}. {address:#018x}  in {module}")),
            None => p.line(format_args!("{number:>3}. {address:#018x}")),
        }
        p.line(format_args!(
            "     expected {:#018x}  read {:#018x}  xor {flipped:#018x}",
            fault.expected, fault.actual
        ));
        p.line(format_args!("     bits {}", Bits(flipped)));
        p.line(format_args!("     found by {} on cpu {} in pass {}", record.test, record.cpu, record.pass + 1));
    }
}

/// The CPUID brand string with whatever is not printable ASCII shown as `?`, so a report is ASCII.
struct Ascii<'a>(&'a str);

impl fmt::Display for Ascii<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.chars().try_for_each(|c| f.write_char(if c.is_ascii_graphic() || c == ' ' { c } else { '?' }))
    }
}

/// The module whose address band holds `address`, when the firmware gave each its own band.
fn module_of(layout: &Layout, address: u64) -> Option<&str> {
    let Layout::Bands(bands) = layout else { return None };
    bands.iter().find(|b| b.extent.contains(address)).map(|b| b.label.as_str())
}

/// One table of finished tests.
fn table(p: &mut Page<'_>, tests: &[&TestRecord]) {
    if tests.is_empty() {
        return;
    }
    p.line(format_args!("  {:<NAME_WIDTH$} {:>4} {:>9} {:>10} {:>7}", "test", "cpus", "time", "rate", "faults"));
    for record in tests {
        let rate = record.rate().map(format::rate);
        p.line(format_args!(
            "  {:<NAME_WIDTH$} {:>4} {:>9} {:>10} {:>7}",
            record.test,
            record.cpus,
            format::took(record.took).as_str(),
            rate.as_ref().map_or("-", |r| r.as_str()),
            record.faults
        ));
    }
}

/// The logical processors split tests ran on.
struct Cores<'a>(&'a [crate::report::CpuInfo]);

impl fmt::Display for Cores<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return f.write_str("1 logical processor (the boot processor alone)");
        }
        let mut cores: Vec<(u32, u32)> = self.0.iter().map(|c| (c.package, c.core)).collect();
        cores.sort_unstable();
        cores.dedup();
        let n = self.0.len();
        write!(f, "{n} logical processor{} on {} core{}", format::plural(n), cores.len(), format::plural(cores.len()))
    }
}

/// The goal and whether it has been met.
struct GoalLine<'a> {
    outlook: &'a Outlook,
    report: &'a Report<'a>,
}

impl fmt::Display for GoalLine<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let goal = self.outlook.goal;
        write!(f, "{goal}")?;
        if goal.is_unlimited() {
            return Ok(());
        }
        let elapsed = self.report.elapsed().unwrap_or(Duration::ZERO);
        match goal.met(self.outlook.settled, elapsed, self.report.snapshot.status.faults) {
            Some(met) => write!(f, "; met ({met})"),
            None => f.write_str("; not met"),
        }
    }
}

/// The positions of the set bits of a mask, lowest first.
struct Bits(u64);

impl fmt::Display for Bits {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut set = (0..64).filter(|bit| self.0 >> bit & 1 == 1);
        for (i, bit) in set.by_ref().take(BITS_LISTED).enumerate() {
            write!(f, "{}{bit}", if i == 0 { "" } else { ", " })?;
        }
        if set.next().is_some() { f.write_str(", ...") } else { Ok(()) }
    }
}

#[cfg(test)]
mod tests;
