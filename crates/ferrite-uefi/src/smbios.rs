//! The SMBIOS structure table, and the memory structures in it: physical memory arrays (type 16),
//! memory devices (17) and the address ranges mapped to them (19 and 20).
//!
//! The table is firmware's data, so nothing in it is trusted: [`Structures`] stops at the first
//! structure that does not fit in the bytes left, every field is read with a bounds check, and a
//! field the structure is too short to have reads as absent. A table that lies gives fewer
//! structures or fewer facts, never a panic.
//!
//! Field offsets follow the DMTF SMBIOS 3.x layouts. The ECC decode (a type 16 correction type, a
//! type 17 total width above its data width) is checked against the layouts only, not against a
//! machine that has ECC memory.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use core::num::{NonZeroU32, NonZeroU64};

use ferrite_hal::PhysAddr;

use crate::memmap::Extent;

/// The end-of-table structure type.
const END_OF_TABLE: u8 = 127;
const TYPE_MEMORY_ARRAY: u8 = 16;
const TYPE_MEMORY_DEVICE: u8 = 17;
const TYPE_ARRAY_RANGE: u8 = 19;
const TYPE_DEVICE_RANGE: u8 = 20;
/// Entries of each kind kept, so a table full of memory structures cannot grow the lists without bound.
const MAX_ENTRIES: usize = 512;
/// The longest string kept; firmware pads some fields to a fixed width.
const MAX_STRING: usize = 64;

/// A structure's handle: the number other structures refer to it by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Handle(pub u16);

/// One structure of the table: its formatted area (header included) and its string set.
#[derive(Clone, Copy, Debug)]
pub struct Structure<'a> {
    pub kind: u8,
    pub handle: Handle,
    formatted: &'a [u8],
    /// The NUL-separated strings, without the final terminator.
    strings: &'a [u8],
}

impl Structure<'_> {
    /// The byte at `offset` in the formatted area; `None` when the structure is shorter.
    #[must_use]
    pub fn u8_at(&self, offset: usize) -> Option<u8> {
        self.formatted.get(offset).copied()
    }

    #[must_use]
    pub fn u16_at(&self, offset: usize) -> Option<u16> {
        self.formatted.get(offset..offset.checked_add(2)?)?.try_into().ok().map(u16::from_le_bytes)
    }

    #[must_use]
    pub fn u32_at(&self, offset: usize) -> Option<u32> {
        self.formatted.get(offset..offset.checked_add(4)?)?.try_into().ok().map(u32::from_le_bytes)
    }

    #[must_use]
    pub fn u64_at(&self, offset: usize) -> Option<u64> {
        self.formatted.get(offset..offset.checked_add(8)?)?.try_into().ok().map(u64::from_le_bytes)
    }

    /// `len` bytes of the formatted area from `offset`; `None` when the structure is shorter.
    #[must_use]
    pub fn bytes_at(&self, offset: usize, len: usize) -> Option<&[u8]> {
        self.formatted.get(offset..offset.checked_add(len)?)
    }

    /// The length of the formatted area.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.formatted.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.formatted.is_empty()
    }

    /// The string the 1-based `index` names, in printable ASCII and trimmed. Index 0 means none, and
    /// so does an index past the set or a string with nothing in it.
    #[must_use]
    pub fn string(&self, index: u8) -> Option<String> {
        let raw = self.strings.split(|&b| b == 0).nth(usize::from(index.checked_sub(1)?))?;
        let text: String = raw.iter().filter(|b| b.is_ascii_graphic() || **b == b' ').map(|&b| char::from(b)).collect();
        let text = text.trim();
        let kept = text.get(..text.len().min(MAX_STRING)).unwrap_or(text);
        (!kept.is_empty()).then(|| kept.into())
    }

    /// The string named by the index byte at `offset`.
    fn string_at(&self, offset: usize) -> Option<String> {
        self.string(self.u8_at(offset)?)
    }
}

/// The structures of a table in order, up to the end marker. Ends at the first structure that is
/// cut off or has a length too short for its own header.
#[derive(Clone, Debug)]
pub struct Structures<'a> {
    rest: &'a [u8],
}

impl<'a> Structures<'a> {
    #[must_use]
    pub const fn new(table: &'a [u8]) -> Self {
        Self { rest: table }
    }

    fn step(&mut self) -> Option<Structure<'a>> {
        let &[kind, len, h0, h1] = self.rest.get(..4)? else { return None };
        let len = usize::from(len);
        if kind == END_OF_TABLE || len < 4 {
            return None;
        }
        let formatted = self.rest.get(..len)?;
        let after = &self.rest[len..];
        // The strings end with two NULs; a structure with none has just those two.
        let end = after.windows(2).position(|w| w == [0, 0])?;
        self.rest = &after[end + 2..];
        Some(Structure { kind, handle: Handle(u16::from_le_bytes([h0, h1])), formatted, strings: &after[..end] })
    }
}

impl<'a> Iterator for Structures<'a> {
    type Item = Structure<'a>;

    fn next(&mut self) -> Option<Structure<'a>> {
        let next = self.step();
        if next.is_none() {
            self.rest = &[];
        }
        next
    }
}

/// The size of a memory device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Size {
    /// The slot has no module in it.
    Empty,
    /// The firmware does not know.
    Unknown,
    Bytes(NonZeroU64),
}

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (bytes, units) = match self {
            Self::Empty => return f.write_str("empty"),
            Self::Unknown => return f.write_str("unknown size"),
            Self::Bytes(bytes) => (bytes.get(), [(30, "GiB"), (20, "MiB"), (10, "KiB"), (0, "B")]),
        };
        let (shift, unit) = units.into_iter().find(|&(shift, _)| bytes.trailing_zeros() >= shift).unwrap_or((0, "B"));
        write!(f, "{} {unit}", bytes >> shift)
    }
}

impl Size {
    /// The size the type 17 size field says, with the extended size field (megabytes, in the low 31
    /// bits) that it points to by holding 0x7fff.
    fn decode(raw: u16, extended: Option<u32>) -> Self {
        let bytes = |count: u64, unit: u32| NonZeroU64::new(count << unit).map_or(Self::Empty, Self::Bytes);
        match raw {
            0xffff => Self::Unknown,
            0 => Self::Empty,
            0x7fff => extended
                .map(|e| u64::from(e & 0x7fff_ffff) << 20)
                .and_then(NonZeroU64::new)
                .map_or(Self::Unknown, Self::Bytes),
            raw if raw & 0x8000 != 0 => bytes(u64::from(raw & 0x7fff), 10),
            raw => bytes(u64::from(raw), 20),
        }
    }
}

/// A speed in mega-transfers per second, never zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MegaTransfers(NonZeroU32);

impl MegaTransfers {
    #[must_use]
    pub const fn new(rate: u32) -> Option<Self> {
        match NonZeroU32::new(rate) {
            Some(rate) => Some(Self(rate)),
            None => None,
        }
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0.get()
    }

    /// The speed a type 17 field says: 0 is unknown, and 0xffff sends to the extended field.
    fn decode(raw: u16, extended: Option<u32>) -> Option<Self> {
        match raw {
            0xffff => extended.and_then(|e| Self::new(e & 0x7fff_ffff)),
            raw => Self::new(u32::from(raw)),
        }
    }
}

impl fmt::Display for MegaTransfers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The technology of a memory device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryKind {
    Other,
    Unspecified,
    Dram,
    Sdram,
    Ddr,
    Ddr2,
    Ddr3,
    Ddr4,
    Ddr5,
    Lpddr,
    Lpddr2,
    Lpddr3,
    Lpddr4,
    Lpddr5,
    Hbm,
    Hbm2,
    Hbm3,
    /// A code this parser has no name for.
    Unknown(u8),
}

impl MemoryKind {
    #[must_use]
    pub const fn from_raw(raw: u8) -> Self {
        match raw {
            0x01 => Self::Other,
            0x02 => Self::Unspecified,
            0x03 => Self::Dram,
            0x0f => Self::Sdram,
            0x12 => Self::Ddr,
            0x13 => Self::Ddr2,
            0x18 => Self::Ddr3,
            0x1a => Self::Ddr4,
            0x1b => Self::Lpddr,
            0x1c => Self::Lpddr2,
            0x1d => Self::Lpddr3,
            0x1e => Self::Lpddr4,
            0x20 => Self::Hbm,
            0x21 => Self::Hbm2,
            0x22 => Self::Ddr5,
            0x23 => Self::Lpddr5,
            0x24 => Self::Hbm3,
            raw => Self::Unknown(raw),
        }
    }

    /// The name, `None` for a code with none.
    #[must_use]
    pub const fn name(self) -> Option<&'static str> {
        Some(match self {
            Self::Other => "other",
            Self::Unspecified => "unspecified",
            Self::Dram => "DRAM",
            Self::Sdram => "SDRAM",
            Self::Ddr => "DDR",
            Self::Ddr2 => "DDR2",
            Self::Ddr3 => "DDR3",
            Self::Ddr4 => "DDR4",
            Self::Ddr5 => "DDR5",
            Self::Lpddr => "LPDDR",
            Self::Lpddr2 => "LPDDR2",
            Self::Lpddr3 => "LPDDR3",
            Self::Lpddr4 => "LPDDR4",
            Self::Lpddr5 => "LPDDR5",
            Self::Hbm => "HBM",
            Self::Hbm2 => "HBM2",
            Self::Hbm3 => "HBM3",
            Self::Unknown(_) => return None,
        })
    }
}

impl fmt::Display for MemoryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.name(), self) {
            (Some(name), _) => f.write_str(name),
            (None, Self::Unknown(raw)) => write!(f, "type {raw:#x}"),
            (None, _) => f.write_str("unknown"),
        }
    }
}

/// How a memory device is packaged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormFactor {
    Other,
    Unspecified,
    Dimm,
    SoDimm,
    Chip,
    /// Chips soldered to the board, as in most laptops with LPDDR.
    RowOfChips,
    Die,
    Camm,
    /// A code this parser has no name for.
    Unknown(u8),
}

impl FormFactor {
    #[must_use]
    pub const fn from_raw(raw: u8) -> Self {
        match raw {
            0x01 => Self::Other,
            0x02 => Self::Unspecified,
            0x05 => Self::Chip,
            0x09 => Self::Dimm,
            0x0d => Self::SoDimm,
            0x0b => Self::RowOfChips,
            0x10 => Self::Die,
            0x11 => Self::Camm,
            raw => Self::Unknown(raw),
        }
    }
}

impl fmt::Display for FormFactor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Other => f.write_str("other"),
            Self::Unspecified => f.write_str("unspecified"),
            Self::Dimm => f.write_str("DIMM"),
            Self::SoDimm => f.write_str("SODIMM"),
            Self::Chip => f.write_str("chip"),
            Self::RowOfChips => f.write_str("soldered"),
            Self::Die => f.write_str("die"),
            Self::Camm => f.write_str("CAMM"),
            Self::Unknown(raw) => write!(f, "form {raw:#x}"),
        }
    }
}

/// The error correction a physical memory array says it has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorCorrection {
    Other,
    Unspecified,
    None,
    Parity,
    SingleBitEcc,
    MultiBitEcc,
    Crc,
    /// A code the specification does not define.
    Unknown(u8),
}

impl ErrorCorrection {
    #[must_use]
    pub const fn from_raw(raw: u8) -> Self {
        match raw {
            0x01 => Self::Other,
            0x02 => Self::Unspecified,
            0x03 => Self::None,
            0x04 => Self::Parity,
            0x05 => Self::SingleBitEcc,
            0x06 => Self::MultiBitEcc,
            0x07 => Self::Crc,
            raw => Self::Unknown(raw),
        }
    }

    /// Whether the platform says it can correct errors; `None` when it does not say.
    #[must_use]
    pub const fn corrects(self) -> Option<bool> {
        match self {
            Self::SingleBitEcc | Self::MultiBitEcc | Self::Crc => Some(true),
            Self::None | Self::Parity => Some(false),
            Self::Other | Self::Unspecified | Self::Unknown(_) => None,
        }
    }
}

impl fmt::Display for ErrorCorrection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Other => f.write_str("other"),
            Self::Unspecified => f.write_str("unknown"),
            Self::None => f.write_str("none"),
            Self::Parity => f.write_str("parity"),
            Self::SingleBitEcc => f.write_str("single-bit ECC"),
            Self::MultiBitEcc => f.write_str("multi-bit ECC"),
            Self::Crc => f.write_str("CRC"),
            Self::Unknown(raw) => write!(f, "code {raw:#x}"),
        }
    }
}

/// A physical memory array (type 16): a set of slots on the board.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryArray {
    pub handle: Handle,
    /// Whether the array is the system's main memory, rather than video or flash memory.
    pub system: bool,
    pub correction: ErrorCorrection,
    /// The number of slots, empty ones included.
    pub slots: u16,
}

impl MemoryArray {
    fn parse(s: &Structure<'_>) -> Option<Self> {
        if s.len() < 0x0f {
            return None;
        }
        Some(Self {
            handle: s.handle,
            system: s.u8_at(0x05)? == 0x03,
            correction: ErrorCorrection::from_raw(s.u8_at(0x06)?),
            slots: s.u16_at(0x0d)?,
        })
    }
}

/// A memory device (type 17): one slot, populated or not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryDevice {
    pub handle: Handle,
    /// The array the slot belongs to.
    pub array: Handle,
    pub locator: Option<String>,
    pub bank: Option<String>,
    pub size: Size,
    pub kind: MemoryKind,
    pub form_factor: FormFactor,
    /// The module's rated speed.
    pub speed: Option<MegaTransfers>,
    /// The speed it runs at now.
    pub configured_speed: Option<MegaTransfers>,
    pub manufacturer: Option<String>,
    pub part_number: Option<String>,
    pub ranks: Option<u8>,
    /// Bits on the data bus, 0 and 0xffff (unknown) as `None`.
    pub data_width: Option<u16>,
    /// Bits including check bits.
    pub total_width: Option<u16>,
}

impl MemoryDevice {
    fn parse(s: &Structure<'_>) -> Option<Self> {
        if s.len() < 0x15 {
            return None;
        }
        let width = |offset| s.u16_at(offset).filter(|w| !matches!(w, 0 | 0xffff));
        Some(Self {
            handle: s.handle,
            array: Handle(s.u16_at(0x04)?),
            total_width: width(0x08),
            data_width: width(0x0a),
            size: Size::decode(s.u16_at(0x0c)?, s.u32_at(0x1c)),
            form_factor: FormFactor::from_raw(s.u8_at(0x0e)?),
            locator: s.string_at(0x10),
            bank: s.string_at(0x11),
            kind: MemoryKind::from_raw(s.u8_at(0x12)?),
            speed: s.u16_at(0x15).and_then(|raw| MegaTransfers::decode(raw, s.u32_at(0x54))),
            manufacturer: s.string_at(0x17),
            part_number: s.string_at(0x1a),
            ranks: s.u8_at(0x1b).map(|attributes| attributes & 0x0f).filter(|&r| r != 0),
            configured_speed: s.u16_at(0x20).and_then(|raw| MegaTransfers::decode(raw, s.u32_at(0x58))),
        })
    }

    /// Whether the slot holds a module.
    #[must_use]
    pub const fn populated(&self) -> bool {
        !matches!(self.size, Size::Empty)
    }

    /// Whether the module carries check bits, from its total width exceeding its data width; `None`
    /// when either width is unknown.
    #[must_use]
    pub fn has_check_bits(&self) -> Option<bool> {
        Some(self.total_width? > self.data_width?)
    }
}

/// Where a physical memory array is mapped (type 19).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArrayRange {
    pub handle: Handle,
    pub array: Handle,
    pub extent: Extent,
}

/// Where one memory device is mapped (type 20), inside an array's range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceRange {
    pub device: Handle,
    /// The type 19 structure this range is part of.
    pub array_range: Handle,
    pub extent: Extent,
    /// The device's place in an interleave: 0 is not interleaved, `None` unknown.
    pub position: Option<u8>,
    /// The rows read from the device before the next one in the interleave: 0 is not interleaved,
    /// `None` unknown.
    pub depth: Option<u8>,
}

/// An address range in kilobytes (inclusive of its last one), or the extended range in bytes
/// (inclusive of its last byte) the 0xffffffff start points to, as an extent.
fn range(start_kb: u32, end_kb: u32, extended: Option<(u64, u64)>) -> Option<Extent> {
    let (start, end) = if start_kb == u32::MAX {
        let (start, last) = extended?;
        (start, last.checked_add(1)?)
    } else if start_kb == 0 && end_kb == 0 {
        // An unmapped entry; a real range is not one kilobyte at address 0.
        return None;
    } else {
        (u64::from(start_kb) << 10, (u64::from(end_kb) + 1) << 10)
    };
    let bytes = end.checked_sub(start).filter(|&b| b > 0)?;
    Some(Extent::new(PhysAddr::new(start), bytes))
}

impl ArrayRange {
    fn parse(s: &Structure<'_>) -> Option<Self> {
        if s.len() < 0x0f {
            return None;
        }
        let extended = s.u64_at(0x0f).zip(s.u64_at(0x17));
        Some(Self {
            handle: s.handle,
            array: Handle(s.u16_at(0x0c)?),
            extent: range(s.u32_at(0x04)?, s.u32_at(0x08)?, extended)?,
        })
    }
}

impl DeviceRange {
    fn parse(s: &Structure<'_>) -> Option<Self> {
        if s.len() < 0x13 {
            return None;
        }
        let extended = s.u64_at(0x13).zip(s.u64_at(0x1b));
        let unless_unknown = |v: u8| (v != 0xff).then_some(v);
        Some(Self {
            device: Handle(s.u16_at(0x0c)?),
            array_range: Handle(s.u16_at(0x0e)?),
            extent: range(s.u32_at(0x04)?, s.u32_at(0x08)?, extended)?,
            position: s.u8_at(0x11).and_then(unless_unknown),
            depth: s.u8_at(0x12).and_then(unless_unknown),
        })
    }
}

/// The memory structures of a table.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Memory {
    pub arrays: Vec<MemoryArray>,
    pub devices: Vec<MemoryDevice>,
    pub array_ranges: Vec<ArrayRange>,
    pub device_ranges: Vec<DeviceRange>,
}

fn keep<T>(list: &mut Vec<T>, item: Option<T>) {
    if let Some(item) = item
        && list.len() < MAX_ENTRIES
    {
        list.push(item);
    }
}

impl Memory {
    /// Every memory structure in `table` that parses.
    #[must_use]
    pub fn parse(table: &[u8]) -> Self {
        let mut memory = Self::default();
        for s in Structures::new(table) {
            match s.kind {
                TYPE_MEMORY_ARRAY => keep(&mut memory.arrays, MemoryArray::parse(&s)),
                TYPE_MEMORY_DEVICE => keep(&mut memory.devices, MemoryDevice::parse(&s)),
                TYPE_ARRAY_RANGE => keep(&mut memory.array_ranges, ArrayRange::parse(&s)),
                TYPE_DEVICE_RANGE => keep(&mut memory.device_ranges, DeviceRange::parse(&s)),
                _ => {},
            }
        }
        memory
    }

    /// The range type 20 gives `device`, if it gives one.
    #[must_use]
    pub fn range_of(&self, device: Handle) -> Option<&DeviceRange> {
        self.device_ranges.iter().find(|r| r.device == device)
    }

    /// The array that is the system's main memory: the first one marked so, else the first.
    #[must_use]
    pub fn main_array(&self) -> Option<&MemoryArray> {
        self.arrays.iter().find(|a| a.system).or_else(|| self.arrays.first())
    }
}

#[cfg(test)]
mod tests;
