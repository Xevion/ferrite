//! What the machine is made of and how it is doing, for the screen, the status page and the log.
//!
//! [`Facts`] are read once at start: the CPU's identity and the firmware's memory description
//! (SMBIOS, see [`crate::smbios`]) with the summaries drawn from it. [`Vitals`] is the clock and the
//! temperature the main loop samples about once a second, in the steps the screens show (whole
//! degrees, tens of MHz) so that a figure that has not changed on screen does not redraw it.
//! [`Hardware`] holds both and is shared between the loop that samples and the status page.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::Cell;
use core::fmt::{self, Write as _};

use ferrite_efi::sensors::{Identity, Reading, TempSource};

use crate::machine::{self, Machine};
use crate::memmap::Extent;
use crate::smbios::{ErrorCorrection, Memory, MemoryDevice, Size};

/// One module's place in physical memory, labelled by its slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Band {
    pub label: String,
    pub extent: Extent,
}

/// How the populated modules are laid out in the address space, from the type 20 ranges.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Layout {
    /// The firmware gave no range for any populated module.
    Unknown,
    /// One module has a range.
    Single,
    /// Every module maps the same range: the controller interleaves them.
    Interleaved { ways: usize },
    /// The modules have ranges of their own, in the order of their slots.
    Bands(Vec<Band>),
}

impl Layout {
    fn of(memory: &Memory) -> Self {
        let bands: Vec<Band> = memory
            .devices
            .iter()
            .filter(|d| d.populated())
            .filter_map(|d| {
                let range = memory.range_of(d.handle)?;
                Some(Band { label: slot_name(d), extent: range.extent })
            })
            .collect();
        match bands.as_slice() {
            [] => Self::Unknown,
            [_] => Self::Single,
            [first, rest @ ..] if rest.iter().all(|b| b.extent == first.extent) => {
                Self::Interleaved { ways: bands.len() }
            },
            _ => Self::Bands(bands),
        }
    }

    /// The one line the memory map carries, when the layout says something.
    #[must_use]
    pub fn caption(&self) -> Option<String> {
        match self {
            Self::Unknown | Self::Single => None,
            Self::Interleaved { ways } => Some(format!("{ways} channels interleaved")),
            Self::Bands(bands) => Some(format!("{} modules, separate ranges", bands.len())),
        }
    }
}

/// A module's slot as its firmware names it.
pub fn slot_name(device: &MemoryDevice) -> String {
    device.locator.clone().or_else(|| device.bank.clone()).unwrap_or_else(|| format!("slot {:#x}", device.handle.0))
}

/// What the machine is, read once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Facts {
    pub cpu: Identity,
    /// The brand as the screen shows it: no vendor, trademarks, clock or core count.
    pub cpu_short: String,
    pub memory: Memory,
    /// The populated modules in one line, see [`modules_text`]; empty when the table lists none.
    pub modules: String,
    pub layout: Layout,
    /// The line the memory map carries for the layout.
    pub caption: Option<String>,
}

impl Facts {
    #[must_use]
    pub fn new(cpu: Identity, memory: Memory) -> Self {
        let cpu_short = short_brand(&cpu.brand);
        let modules = modules_text(&memory);
        let layout = Layout::of(&memory);
        let caption = layout.caption();
        Self { cpu, cpu_short, memory, modules, layout, caption }
    }

    /// The facts of this machine, from CPUID and the firmware's SMBIOS table; logs the modules and
    /// the ECC claim.
    #[must_use]
    pub fn detect(cpu: Identity) -> Self {
        let table = machine::smbios_table();
        if table.is_none() {
            log::info!("memory: the firmware publishes no SMBIOS table");
        }
        let facts = Self::new(cpu, table.as_deref().map_or_default(Memory::parse));
        facts.log_memory();
        facts
    }

    fn log_memory(&self) {
        let memory = &self.memory;
        if memory.devices.is_empty() && !memory.arrays.is_empty() {
            log::info!("memory: {} arrays but no memory devices in the SMBIOS table", memory.arrays.len());
        }
        for device in &memory.devices {
            log::info!("memory: {}", ModuleLine(device));
        }
        match self.ecc_claim() {
            Some(claim) => log::info!("memory: the firmware claims error correction: {claim}"),
            None => log::info!("memory: the firmware makes no claim about error correction"),
        }
        match (self.caption.as_deref(), &self.layout) {
            (Some(caption), Layout::Bands(bands)) => {
                log::info!("memory: {caption}");
                for band in bands {
                    log::info!(
                        "memory: {} at {:#x}, {} bytes",
                        band.label,
                        band.extent.start().get(),
                        bytes_of(band.extent)
                    );
                }
            },
            (Some(caption), _) => log::info!("memory: {caption}"),
            (None, _) => log::info!("memory: no per-module address ranges"),
        }
    }

    /// What the platform says about error correction, from the main memory array; `None` with no array.
    #[must_use]
    pub fn ecc_claim(&self) -> Option<ErrorCorrection> {
        self.memory.main_array().map(|a| a.correction)
    }

    /// Whether the platform says it corrects errors: `Some(true)` for ECC, `Some(false)` for none or
    /// parity, `None` when it does not say.
    #[must_use]
    pub fn corrects_errors(&self) -> Option<bool> {
        self.ecc_claim().and_then(ErrorCorrection::corrects)
    }
}

/// The modules in one line, identical ones grouped: `2 x 8 GiB LPDDR3-2133`, with the empty slots
/// counted after. Empty when the table lists no module.
#[must_use]
pub fn modules_text(memory: &Memory) -> String {
    let mut groups: Vec<(Size, String, usize)> = Vec::new();
    let mut empty = 0;
    for device in &memory.devices {
        if !device.populated() {
            empty += 1;
            continue;
        }
        let kind = module_kind(device);
        match groups.iter_mut().find(|(size, k, _)| *size == device.size && *k == kind) {
            Some((_, _, count)) => *count += 1,
            None => groups.push((device.size, kind, 1)),
        }
    }
    let mut out = String::new();
    for (i, (size, kind, count)) in groups.iter().enumerate() {
        let sep = if i == 0 { "" } else { " + " };
        let _ = write!(out, "{sep}{count} x {size} {kind}");
    }
    if empty > 0 {
        let _ = write!(out, "{}{empty} empty", if out.is_empty() { "" } else { ", " });
    }
    out.truncate(out.trim_end().len());
    out
}

/// The technology and speed of a module: `LPDDR3-2133`, or just the technology without a speed.
fn module_kind(device: &MemoryDevice) -> String {
    let mut out = String::new();
    if !matches!(device.kind, crate::smbios::MemoryKind::Unspecified | crate::smbios::MemoryKind::Other) {
        let _ = write!(out, "{}", device.kind);
    }
    if let Some(rate) = device.configured_speed.or(device.speed) {
        let _ = write!(out, "{}{rate}", if out.is_empty() { "" } else { "-" });
    }
    out
}

fn bytes_of(extent: Extent) -> u64 {
    extent.end() - extent.start().get()
}

/// A memory device as one log line.
struct ModuleLine<'a>(&'a MemoryDevice);

impl fmt::Display for ModuleLine<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let d = self.0;
        write!(f, "{}: {}", slot_name(d), d.size)?;
        if d.populated() {
            write!(f, " {} {}", d.kind, d.form_factor)?;
            if let Some(speed) = d.speed {
                write!(f, ", {speed} MT/s")?;
            }
            if let Some(speed) = d.configured_speed {
                write!(f, ", running {speed} MT/s")?;
            }
            if let Some(ranks) = d.ranks {
                write!(f, ", {ranks} rank{}", if ranks == 1 { "" } else { "s" })?;
            }
            if let (Some(total), Some(data)) = (d.total_width, d.data_width) {
                write!(f, ", {data} of {total} bits")?;
            }
            if let Some(part) = &d.part_number {
                write!(f, ", part {part}")?;
            }
            if let Some(maker) = &d.manufacturer {
                write!(f, " by {maker}")?;
            }
        }
        Ok(())
    }
}

/// The brand string as the screen shows it.
///
/// `Intel(R) Core(TM) i7-8650U CPU @ 1.90GHz` becomes `Core i7-8650U` and `AMD Ryzen 7 5800X 8-Core
/// Processor` becomes `Ryzen 7 5800X`. A brand the rules leave empty is returned as it was.
#[must_use]
pub fn short_brand(brand: &str) -> String {
    let plain = brand.replace("(R)", "").replace("(TM)", "").replace("(tm)", "");
    let words: Vec<&str> = plain
        .split_whitespace()
        .take_while(|w| !matches!(*w, "@" | "with"))
        .filter(|w| !w.eq_ignore_ascii_case("intel") && !w.eq_ignore_ascii_case("amd"))
        .filter(|w| !w.eq_ignore_ascii_case("cpu") && !w.eq_ignore_ascii_case("processor"))
        .filter(|w| !w.to_ascii_lowercase().ends_with("-core"))
        .collect();
    if words.is_empty() { brand.trim().into() } else { words.join(" ") }
}

/// The clock and the temperature as the screens show them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Vitals {
    /// The clock in MHz, to the nearest ten.
    pub mhz: Option<u32>,
    /// Whether the clock was measured, rather than the base frequency CPUID states.
    pub measured: bool,
    /// The temperature in whole degrees Celsius.
    pub celsius: Option<i16>,
    pub source: Option<TempSource>,
}

impl Vitals {
    #[must_use]
    pub fn of(reading: Reading) -> Self {
        Self {
            mhz: reading.frequency.map(|f| (f.mhz + 5) / 10 * 10),
            measured: reading.frequency.is_some_and(|f| f.measured),
            celsius: reading.temperature.map(|t| t.celsius),
            source: reading.temperature.map(|t| t.source),
        }
    }
}

/// The machine, shared by the loop that samples it and the pages that show it.
#[derive(Debug)]
pub struct Hardware {
    pub facts: Facts,
    vitals: Cell<Vitals>,
    /// The highest temperature any sample has read, in whole degrees Celsius.
    peak_celsius: Cell<Option<i16>>,
    machine: Cell<Machine>,
}

impl Hardware {
    #[must_use]
    pub const fn new(facts: Facts) -> Self {
        Self {
            facts,
            vitals: Cell::new(Vitals { mhz: None, measured: false, celsius: None, source: None }),
            peak_celsius: Cell::new(None),
            machine: Cell::new(Machine::UNKNOWN),
        }
    }

    #[must_use]
    pub fn vitals(&self) -> Vitals {
        self.vitals.get()
    }

    pub fn set_vitals(&self, vitals: Vitals) {
        self.vitals.set(vitals);
        self.peak_celsius.set(self.peak_celsius.get().max(vitals.celsius));
    }

    /// The highest temperature read since start, if any sample had one.
    #[must_use]
    pub fn peak_celsius(&self) -> Option<i16> {
        self.peak_celsius.get()
    }

    /// Which machine this is, as the timings are keyed.
    #[must_use]
    pub fn machine(&self) -> Machine {
        self.machine.get()
    }

    pub fn set_machine(&self, machine: Machine) {
        self.machine.set(machine);
    }
}

impl Default for Hardware {
    /// A machine that is known to be nothing: for hosts and tests with nothing to ask.
    fn default() -> Self {
        let cpu = Identity {
            vendor: ferrite_power::cpuid::Vendor::Other([0; 12]),
            signature: ferrite_power::cpuid::Signature { family: 0, model: 0, stepping: 0 },
            brand: String::new(),
        };
        Self::new(Facts::new(cpu, Memory::default()))
    }
}

#[cfg(test)]
mod tests;
