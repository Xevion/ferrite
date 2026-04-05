use std::fmt;
use std::fs;

use serde::Serialize;

/// SMBIOS memory type codes from the Type 17 structure (offset 0x12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryType {
    Other,
    Ddr,
    Ddr2,
    Ddr3,
    Ddr4,
    Ddr5,
    Lpddr5,
    /// Unrecognized SMBIOS type code; raw byte preserved.
    Unknown(u8),
}

impl From<u8> for MemoryType {
    fn from(byte: u8) -> Self {
        match byte {
            0x01 => Self::Other,
            0x12 => Self::Ddr,
            0x13 => Self::Ddr2,
            0x18 => Self::Ddr3,
            0x1A => Self::Ddr4,
            0x22 => Self::Ddr5,
            0x23 => Self::Lpddr5,
            b => Self::Unknown(b),
        }
    }
}

impl fmt::Display for MemoryType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Other => f.write_str("Other"),
            Self::Ddr => f.write_str("DDR"),
            Self::Ddr2 => f.write_str("DDR2"),
            Self::Ddr3 => f.write_str("DDR3"),
            Self::Ddr4 => f.write_str("DDR4"),
            Self::Ddr5 => f.write_str("DDR5"),
            Self::Lpddr5 => f.write_str("LPDDR5"),
            Self::Unknown(b) => write!(f, "Unknown(0x{b:02X})"),
        }
    }
}

impl Serialize for MemoryType {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

/// SMBIOS Type 17 (Memory Device) information for a single DIMM slot.
#[derive(Debug, Clone, Serialize)]
pub struct DimmInfo {
    pub handle: u16,
    /// Silk-screen label, e.g., "`DIMM_A1`".
    pub device_locator: String,
    /// Bank grouping label, e.g., "BANK 0" or "`P0_Node0_Channel0_Dimm0`".
    pub bank_locator: String,
    pub manufacturer: Option<String>,
    pub serial_number: Option<String>,
    pub part_number: Option<String>,
    pub size_mb: u64,
    pub memory_type: MemoryType,
    pub speed_mhz: u16,
}

/// Parse SMBIOS Type 17 entries from the raw DMI table.
/// Returns `None` if the DMI table cannot be read.
#[must_use]
pub fn read_dimm_info() -> Option<Vec<DimmInfo>> {
    let table = fs::read("/sys/firmware/dmi/tables/DMI").ok()?;
    let dimms = parse_type17_entries(&table);
    if dimms.is_empty() { None } else { Some(dimms) }
}

/// Parse all Type 17 structures from a raw SMBIOS table blob.
pub(crate) fn parse_type17_entries(table: &[u8]) -> Vec<DimmInfo> {
    let mut dimms = Vec::new();
    let mut offset = 0;

    while offset + 4 <= table.len() {
        let struct_type = table[offset];
        let struct_len = table[offset + 1] as usize;

        if struct_len < 4 || offset + struct_len > table.len() {
            break;
        }

        // Find the string table: starts at offset + struct_len, terminated by double NUL
        let strings_start = offset + struct_len;
        let strings_end = find_string_table_end(table, strings_start);
        let strings = &table[strings_start..strings_end];

        if struct_type == 17 && struct_len >= 0x17 {
            let handle = u16::from_le_bytes([table[offset + 2], table[offset + 3]]);
            let size_raw = u16::from_le_bytes([table[offset + 0x0C], table[offset + 0x0D]]);
            let memory_type_byte = table[offset + 0x12];
            let speed = u16::from_le_bytes([table[offset + 0x15], table[offset + 0x16]]);

            let device_locator_idx = table[offset + 0x10];
            let bank_locator_idx = table[offset + 0x11];

            let manufacturer_idx = if struct_len > 0x17 {
                table[offset + 0x17]
            } else {
                0
            };
            let serial_idx = if struct_len > 0x18 {
                table[offset + 0x18]
            } else {
                0
            };
            let part_idx = if struct_len > 0x1A {
                table[offset + 0x1A]
            } else {
                0
            };

            // 0x7FFF = use 32-bit extended size at 0x1C (requires struct_len >= 0x20)
            let ext_bytes = (struct_len >= 0x20).then(|| {
                [
                    table[offset + 0x1C],
                    table[offset + 0x1D],
                    table[offset + 0x1E],
                    table[offset + 0x1F],
                ]
            });
            let size_mb = parse_size_mb(size_raw, ext_bytes);

            // Skip empty slots
            if size_mb == 0 {
                offset = strings_end;
                continue;
            }

            dimms.push(DimmInfo {
                handle,
                device_locator: smbios_string(strings, device_locator_idx).unwrap_or_default(),
                bank_locator: smbios_string(strings, bank_locator_idx).unwrap_or_default(),
                manufacturer: smbios_string(strings, manufacturer_idx),
                serial_number: smbios_string(strings, serial_idx),
                part_number: smbios_string(strings, part_idx),
                size_mb,
                memory_type: MemoryType::from(memory_type_byte),
                speed_mhz: speed,
            });
        }

        // End-of-table marker
        if struct_type == 127 {
            break;
        }

        offset = strings_end;
    }

    dimms
}

/// Find the end of the SMBIOS string table (double NUL terminator), returning the
/// byte position immediately after it. The string area is a sequence of NUL-terminated
/// strings followed by a terminating NUL, so we scan for `\0\0`.
pub(crate) fn find_string_table_end(table: &[u8], start: usize) -> usize {
    table[start..]
        .windows(2)
        .position(|w| w == [0, 0])
        .map_or(table.len(), |p| start + p + 2)
}

/// Decode the SMBIOS size field into megabytes.
///
/// - `0x0000` / `0xFFFF` -- slot not installed or size unknown, returns 0.
/// - `0x7FFF` -- use 32-bit extended size from `ext_bytes`; returns 0 if absent.
/// - bit 15 set -- KB granularity; value is the low 15 bits divided by 1024.
/// - otherwise -- MB granularity.
fn parse_size_mb(size_raw: u16, ext_bytes: Option<[u8; 4]>) -> u64 {
    match size_raw {
        0 | 0xFFFF => 0,
        0x7FFF => ext_bytes.map_or(0, |b| u64::from(u32::from_le_bytes(b))),
        other if other & 0x8000 != 0 => u64::from(other & 0x7FFF) / 1024,
        other => u64::from(other),
    }
}

/// Get the Nth string (1-indexed) from a NUL-terminated SMBIOS string table.
/// Returns `None` for index 0, out-of-range indices, or strings empty after trimming.
fn smbios_string(strings: &[u8], index: u8) -> Option<String> {
    if index == 0 {
        return None;
    }
    let s = strings.split(|&b| b == 0).nth((index - 1) as usize)?;
    let trimmed = String::from_utf8_lossy(s).trim().to_owned();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

#[cfg(test)]
mod tests {
    // Re-export so nested mods can access everything with a single `use super::*`.
    pub use super::*;
    pub use assert2::check;
    pub use proptest::prelude::*;

    mod string_lookup {
        use super::*;

        #[test]
        fn one_indexed() {
            let strings = b"Hello\0World\0Test\0";
            check!(smbios_string(strings, 1) == Some("Hello".to_owned()));
            check!(smbios_string(strings, 2) == Some("World".to_owned()));
            check!(smbios_string(strings, 3) == Some("Test".to_owned()));
        }

        #[test]
        fn index_zero_returns_none() {
            check!(smbios_string(b"Hello\0\0", 0) == None);
        }

        #[test]
        fn out_of_range_returns_none() {
            let strings = b"Hello\0World\0\0";
            check!(smbios_string(strings, 10) == None);
        }

        #[test]
        fn empty_entry_returns_none() {
            // A NUL-only entry between valid strings resolves to None
            let strings = b"\0World\0\0";
            check!(smbios_string(strings, 1) == None);
            check!(smbios_string(strings, 2) == Some("World".to_owned()));
        }

        #[test]
        fn trims_surrounding_whitespace() {
            let strings = b"  hello  \0\0";
            check!(smbios_string(strings, 1) == Some("hello".to_owned()));
        }

        #[test]
        fn whitespace_only_returns_none() {
            let strings = b"   \0real\0\0";
            check!(smbios_string(strings, 1) == None);
            check!(smbios_string(strings, 2) == Some("real".to_owned()));
        }

        #[test]
        fn invalid_utf8_uses_replacement_char() {
            let strings = &[0xFF, 0xFE, 0x00, 0x00];
            let result = smbios_string(strings, 1);
            check!(result.is_some());
            check!(result.unwrap().contains('\u{FFFD}'));
        }

        proptest! {
            #[test]
            fn never_panics(
                bytes in prop::collection::vec(any::<u8>(), 0..=128),
                index: u8,
            ) {
                let _ = smbios_string(&bytes, index);
            }
        }
    }

    mod string_table {
        use super::*;

        #[test]
        fn double_nul_terminates() {
            let data = b"abc\0def\0\0rest";
            check!(find_string_table_end(data, 0) == 9);
        }

        #[test]
        fn start_offset_is_respected() {
            // Double NUL at absolute positions 3,4; starting scan at 3 finds them immediately.
            let data = b"abc\0\0x";
            check!(find_string_table_end(data, 3) == 5);
        }

        #[test]
        fn no_double_nul_returns_len() {
            let data = b"abc\0def";
            check!(find_string_table_end(data, 0) == data.len());
        }

        #[test]
        fn start_at_end_of_table_returns_len() {
            let data = b"abc";
            check!(find_string_table_end(data, data.len()) == data.len());
        }

        #[test]
        fn single_byte_remaining_returns_len() {
            let data = b"abc\0x";
            // Starting at index 4, only one byte left -- can't form a window of 2
            check!(find_string_table_end(data, 4) == data.len());
        }

        proptest! {
            #[test]
            fn never_panics(
                (bytes, start) in prop::collection::vec(any::<u8>(), 0..=256)
                    .prop_flat_map(|v| {
                        let len = v.len();
                        (Just(v), 0..=len)
                    })
            ) {
                let _ = find_string_table_end(&bytes, start);
            }
        }
    }

    mod size_parsing {
        use super::*;

        #[test]
        fn zero_is_empty_slot() {
            check!(parse_size_mb(0x0000, None) == 0);
        }

        #[test]
        fn ffff_is_unknown_size() {
            check!(parse_size_mb(0xFFFF, None) == 0);
        }

        #[test]
        fn mb_granularity() {
            check!(parse_size_mb(0x1000, None) == 4096);
            check!(parse_size_mb(0x2000, None) == 8192);
        }

        #[test]
        fn kb_granularity() {
            // Bit 15 set; low 15 bits are KB. 8192 KB = 8 MB, 1024 KB = 1 MB.
            check!(parse_size_mb(0xA000, None) == 8); // 0x2000 = 8192 KB
            check!(parse_size_mb(0x8400, None) == 1); // 0x0400 = 1024 KB
        }

        #[test]
        fn kb_granularity_sub_1mb_truncates_to_zero() {
            // 0x8001 = bit15 set | 1 KB -> 1/1024 = 0 (truncation bug)
            check!(parse_size_mb(0x8001, None) == 0);
            // 0x8200 = bit15 set | 512 KB -> 512/1024 = 0
            check!(parse_size_mb(0x8200, None) == 0);
        }

        #[test]
        fn kb_granularity_non_even_division() {
            // 0x8300 = bit15 set | 768 KB -> 768/1024 = 0 (truncation)
            check!(parse_size_mb(0x8300, None) == 0);
        }

        #[test]
        fn extended_size() {
            let ext = 32768u32.to_le_bytes();
            check!(parse_size_mb(0x7FFF, Some(ext)) == 32768);
        }

        #[test]
        fn extended_size_without_ext_bytes_returns_zero() {
            check!(parse_size_mb(0x7FFF, None) == 0);
        }
    }

    mod memory_type {
        use super::*;

        #[rstest::rstest]
        #[case(0x01, MemoryType::Other)]
        #[case(0x12, MemoryType::Ddr)]
        #[case(0x13, MemoryType::Ddr2)]
        #[case(0x18, MemoryType::Ddr3)]
        #[case(0x1A, MemoryType::Ddr4)]
        #[case(0x22, MemoryType::Ddr5)]
        #[case(0x23, MemoryType::Lpddr5)]
        #[case(0xFF, MemoryType::Unknown(0xFF))]
        fn from_byte(#[case] code: u8, #[case] expected: MemoryType) {
            check!(MemoryType::from(code) == expected);
        }

        #[rstest::rstest]
        #[case(MemoryType::Other, "Other")]
        #[case(MemoryType::Ddr, "DDR")]
        #[case(MemoryType::Ddr2, "DDR2")]
        #[case(MemoryType::Ddr3, "DDR3")]
        #[case(MemoryType::Ddr4, "DDR4")]
        #[case(MemoryType::Ddr5, "DDR5")]
        #[case(MemoryType::Lpddr5, "LPDDR5")]
        #[case(MemoryType::Unknown(0xFF), "Unknown(0xFF)")]
        #[case(MemoryType::Unknown(0x02), "Unknown(0x02)")]
        fn display(#[case] ty: MemoryType, #[case] expected: &str) {
            check!(ty.to_string() == expected);
        }

        #[test]
        fn serialize_json() {
            let json = serde_json::to_string(&MemoryType::Ddr5).unwrap();
            check!(json == "\"DDR5\"");
            let json = serde_json::to_string(&MemoryType::Unknown(0xAB)).unwrap();
            check!(json == "\"Unknown(0xAB)\"");
        }
    }

    mod type17 {
        use super::*;

        /// Build a minimal valid Type 17 structure with the given size field and
        /// optional extended size, plus an end-of-table marker.
        fn build_type17(size_lo: u8, size_hi: u8, ext_size: Option<u32>) -> Vec<u8> {
            let struct_len = if ext_size.is_some() { 0x20usize } else { 0x1B };
            let mut s = vec![0u8; struct_len];
            s[0] = 17;
            s[1] = struct_len as u8;
            s[0x0C] = size_lo;
            s[0x0D] = size_hi;
            s[0x10] = 1; // device_locator = string 1
            s[0x12] = 0x1A; // DDR4
            s[0x15] = 0x20; // 3200 MHz low
            s[0x16] = 0x0C; // 3200 MHz high
            if let Some(ext) = ext_size {
                let bytes = ext.to_le_bytes();
                s[0x1C] = bytes[0];
                s[0x1D] = bytes[1];
                s[0x1E] = bytes[2];
                s[0x1F] = bytes[3];
            }
            s.extend_from_slice(b"DIMM0\0\0");
            s.extend_from_slice(&[127, 4, 0, 0, 0, 0]);
            s
        }

        #[test]
        fn full_fixture() {
            let mut structure = vec![0u8; 0x1B];
            structure[0] = 17;
            structure[1] = 0x1B;
            structure[2] = 0x20; // handle low
            structure[3] = 0x00; // handle high
            structure[0x0C] = 0x00;
            structure[0x0D] = 0x20; // 8192 MB
            structure[0x10] = 1; // device locator = string 1
            structure[0x11] = 2; // bank locator = string 2
            structure[0x12] = 0x22; // DDR5
            structure[0x15] = 0xC0;
            structure[0x16] = 0x12; // 4800 MHz
            structure[0x17] = 3; // manufacturer = string 3
            structure[0x18] = 4; // serial = string 4
            structure[0x1A] = 5; // part = string 5
            structure.extend_from_slice(b"DIMM_A1\0BANK 0\0Samsung\0SN12345\0M471A2G43AB2\0\0");
            structure.extend_from_slice(&[127, 4, 0xFF, 0xFF, 0, 0]);

            let dimms = parse_type17_entries(&structure);
            check!(dimms.len() == 1);
            let d = &dimms[0];
            check!(d.handle == 0x20);
            check!(d.device_locator == "DIMM_A1");
            check!(d.bank_locator == "BANK 0");
            check!(d.manufacturer.as_deref() == Some("Samsung"));
            check!(d.serial_number.as_deref() == Some("SN12345"));
            check!(d.part_number.as_deref() == Some("M471A2G43AB2"));
            check!(d.size_mb == 8192);
            check!(d.memory_type == MemoryType::Ddr5);
            check!(d.speed_mhz == 4800);
        }

        #[test]
        fn kb_granularity_size() {
            // 0x8000 | 8192 = 0xA000 -> 8192 KB / 1024 = 8 MB
            let table = build_type17(0x00, 0xA0, None);
            let dimms = parse_type17_entries(&table);
            check!(dimms.len() == 1);
            check!(dimms[0].size_mb == 8);
        }

        #[test]
        fn extended_size() {
            // size_raw = 0x7FFF triggers extended size read at 0x1C
            let table = build_type17(0xFF, 0x7F, Some(32768));
            let dimms = parse_type17_entries(&table);
            check!(dimms.len() == 1);
            check!(dimms[0].size_mb == 32768);
        }

        #[test]
        fn size_zero_slot_skipped() {
            let mut structure = vec![0u8; 0x1B];
            structure[0] = 17;
            structure[1] = 0x1B;
            structure.extend_from_slice(&[0, 0]);
            structure.extend_from_slice(&[127, 4, 0, 0, 0, 0]);
            check!(parse_type17_entries(&structure).is_empty());
        }

        #[test]
        fn size_ffff_slot_skipped() {
            let table = build_type17(0xFF, 0xFF, None);
            check!(parse_type17_entries(&table).is_empty());
        }

        #[test]
        fn multiple_entries() {
            let struct_len = 0x1Bu8;
            let mut table = Vec::new();

            let mut s1 = vec![0u8; 0x1B];
            s1[0] = 17;
            s1[1] = struct_len;
            s1[0x0C] = 0x00;
            s1[0x0D] = 0x10; // 4096 MB
            s1[0x10] = 1;
            s1[0x12] = 0x1A; // DDR4
            table.extend_from_slice(&s1);
            table.extend_from_slice(b"SLOT1\0\0");

            let mut s2 = vec![0u8; 0x1B];
            s2[0] = 17;
            s2[1] = struct_len;
            s2[0x0C] = 0x00;
            s2[0x0D] = 0x20; // 8192 MB
            s2[0x10] = 1;
            s2[0x12] = 0x22; // DDR5
            table.extend_from_slice(&s2);
            table.extend_from_slice(b"SLOT2\0\0");

            table.extend_from_slice(&[127, 4, 0, 0, 0, 0]);

            let dimms = parse_type17_entries(&table);
            check!(dimms.len() == 2);
            check!(dimms[0].size_mb == 4096);
            check!(dimms[0].device_locator == "SLOT1");
            check!(dimms[0].memory_type == MemoryType::Ddr4);
            check!(dimms[1].size_mb == 8192);
            check!(dimms[1].memory_type == MemoryType::Ddr5);
        }

        #[test]
        fn minimal_struct_length_no_optional_fields() {
            // Struct length exactly 0x17 -- manufacturer/serial/part offsets are absent
            let mut s = vec![0u8; 0x17];
            s[0] = 17;
            s[1] = 0x17;
            s[0x0C] = 0x00;
            s[0x0D] = 0x10; // 4096 MB
            s[0x10] = 1;
            s[0x12] = 0x1A; // DDR4
            s[0x15] = 0xC0;
            s[0x16] = 0x12; // 4800 MHz
            s.extend_from_slice(b"DIMM0\0\0");
            s.extend_from_slice(&[127, 4, 0, 0, 0, 0]);

            let dimms = parse_type17_entries(&s);
            check!(dimms.len() == 1);
            check!(dimms[0].manufacturer.is_none());
            check!(dimms[0].serial_number.is_none());
            check!(dimms[0].part_number.is_none());
        }

        #[test]
        fn non_type17_structures_skipped() {
            let mut table = Vec::new();

            // Type 1 (System Information) -- should be skipped
            let mut s1 = vec![0u8; 0x1B];
            s1[0] = 1; // Type 1
            s1[1] = 0x1B;
            table.extend_from_slice(&s1);
            table.extend_from_slice(b"SystemInfo\0\0");

            // Type 17 -- should be parsed
            let mut s2 = vec![0u8; 0x1B];
            s2[0] = 17;
            s2[1] = 0x1B;
            s2[0x0C] = 0x00;
            s2[0x0D] = 0x10; // 4096 MB
            s2[0x10] = 1;
            s2[0x12] = 0x1A; // DDR4
            table.extend_from_slice(&s2);
            table.extend_from_slice(b"DIMM0\0\0");

            // Type 4 (Processor) -- should be skipped
            let mut s3 = vec![0u8; 0x1B];
            s3[0] = 4; // Type 4
            s3[1] = 0x1B;
            table.extend_from_slice(&s3);
            table.extend_from_slice(b"CPU0\0\0");

            table.extend_from_slice(&[127, 4, 0, 0, 0, 0]);

            let dimms = parse_type17_entries(&table);
            check!(dimms.len() == 1);
            check!(dimms[0].device_locator == "DIMM0");
        }

        #[test]
        fn type17_too_short_skipped() {
            // Type 17 with struct_len < 0x17 -- not enough data for required fields
            let mut table = Vec::new();
            let mut s = vec![0u8; 0x10]; // only 16 bytes, less than 0x17
            s[0] = 17;
            s[1] = 0x10;
            table.extend_from_slice(&s);
            table.extend_from_slice(b"\0\0");

            table.extend_from_slice(&[127, 4, 0, 0, 0, 0]);

            check!(parse_type17_entries(&table).is_empty());
        }

        #[test]
        fn end_of_table_marker_stops_iteration() {
            let mut table = Vec::new();

            // Type 17 -- should be parsed
            let mut s1 = vec![0u8; 0x1B];
            s1[0] = 17;
            s1[1] = 0x1B;
            s1[0x0C] = 0x00;
            s1[0x0D] = 0x10; // 4096 MB
            s1[0x10] = 1;
            s1[0x12] = 0x1A;
            table.extend_from_slice(&s1);
            table.extend_from_slice(b"SLOT1\0\0");

            // End-of-table marker (type 127)
            table.extend_from_slice(&[127, 4, 0, 0, 0, 0]);

            // Another Type 17 after the marker -- should NOT be parsed
            let mut s2 = vec![0u8; 0x1B];
            s2[0] = 17;
            s2[1] = 0x1B;
            s2[0x0C] = 0x00;
            s2[0x0D] = 0x20; // 8192 MB
            s2[0x10] = 1;
            s2[0x12] = 0x22;
            table.extend_from_slice(&s2);
            table.extend_from_slice(b"SLOT2\0\0");

            let dimms = parse_type17_entries(&table);
            check!(dimms.len() == 1);
            check!(dimms[0].device_locator == "SLOT1");
        }

        #[test]
        fn truncated_table_stops_gracefully() {
            // struct_len claims 0x1B bytes but buffer is shorter
            let mut table = vec![0u8; 0x10];
            table[0] = 17;
            table[1] = 0x1B; // claims 27 bytes but only 16 available
            check!(parse_type17_entries(&table).is_empty());
        }

        #[test]
        fn empty_string_table_gives_defaults() {
            // String table is just \0\0 -- no strings at all
            let mut s = vec![0u8; 0x1B];
            s[0] = 17;
            s[1] = 0x1B;
            s[0x0C] = 0x00;
            s[0x0D] = 0x10; // 4096 MB
            s[0x10] = 1; // device_locator points to string 1, but no strings exist
            s[0x11] = 1; // bank_locator also points to nonexistent string
            s[0x12] = 0x1A;
            s.extend_from_slice(b"\0\0"); // empty string table

            s.extend_from_slice(&[127, 4, 0, 0, 0, 0]);

            let dimms = parse_type17_entries(&s);
            check!(dimms.len() == 1);
            check!(dimms[0].device_locator.is_empty());
            check!(dimms[0].bank_locator.is_empty());
        }

        #[test]
        fn struct_len_0x18_has_manufacturer_only() {
            let mut s = vec![0u8; 0x18];
            s[0] = 17;
            s[1] = 0x18;
            s[0x0C] = 0x00;
            s[0x0D] = 0x10; // 4096 MB
            s[0x10] = 1;
            s[0x12] = 0x1A;
            s[0x17] = 2; // manufacturer = string 2
            s.extend_from_slice(b"DIMM0\0Samsung\0\0");
            s.extend_from_slice(&[127, 4, 0, 0, 0, 0]);

            let dimms = parse_type17_entries(&s);
            check!(dimms.len() == 1);
            check!(dimms[0].manufacturer.as_deref() == Some("Samsung"));
            check!(dimms[0].serial_number.is_none());
            check!(dimms[0].part_number.is_none());
        }

        #[test]
        fn struct_len_0x19_has_manufacturer_and_serial() {
            let mut s = vec![0u8; 0x19];
            s[0] = 17;
            s[1] = 0x19;
            s[0x0C] = 0x00;
            s[0x0D] = 0x10; // 4096 MB
            s[0x10] = 1;
            s[0x12] = 0x1A;
            s[0x17] = 2; // manufacturer = string 2
            s[0x18] = 3; // serial = string 3
            s.extend_from_slice(b"DIMM0\0Samsung\0SN999\0\0");
            s.extend_from_slice(&[127, 4, 0, 0, 0, 0]);

            let dimms = parse_type17_entries(&s);
            check!(dimms.len() == 1);
            check!(dimms[0].manufacturer.as_deref() == Some("Samsung"));
            check!(dimms[0].serial_number.as_deref() == Some("SN999"));
            check!(dimms[0].part_number.is_none());
        }

        #[test]
        fn struct_len_0x1a_still_no_part_number() {
            // struct_len == 0x1A: guard is `struct_len > 0x1A` which is false
            let mut s = vec![0u8; 0x1A];
            s[0] = 17;
            s[1] = 0x1A;
            s[0x0C] = 0x00;
            s[0x0D] = 0x10; // 4096 MB
            s[0x10] = 1;
            s[0x12] = 0x1A;
            s[0x17] = 2;
            s[0x18] = 3;
            // 0x19 is Asset Tag (not parsed), 0x1A would be part but struct_len doesn't cover it
            s.extend_from_slice(b"DIMM0\0Samsung\0SN999\0\0");
            s.extend_from_slice(&[127, 4, 0, 0, 0, 0]);

            let dimms = parse_type17_entries(&s);
            check!(dimms.len() == 1);
            check!(dimms[0].manufacturer.as_deref() == Some("Samsung"));
            check!(dimms[0].serial_number.as_deref() == Some("SN999"));
            check!(dimms[0].part_number.is_none());
        }

        proptest! {
            #[test]
            fn never_panics(bytes in prop::collection::vec(any::<u8>(), 0..=512)) {
                let _ = parse_type17_entries(&bytes);
            }

            /// Generate valid-shaped Type 17 structures with random field values
            /// to exercise more interesting code paths than fully random bytes.
            #[test]
            fn valid_shaped_never_panics(
                size_lo in any::<u8>(),
                size_hi in any::<u8>(),
                mem_type in any::<u8>(),
                speed_lo in any::<u8>(),
                speed_hi in any::<u8>(),
                use_ext in any::<bool>(),
                ext_size in any::<u32>(),
                label in "[a-zA-Z0-9_]{1,16}",
            ) {
                let struct_len: usize = if use_ext { 0x20 } else { 0x1B };
                let mut s = vec![0u8; struct_len];
                s[0] = 17;
                s[1] = struct_len as u8;
                s[0x0C] = size_lo;
                s[0x0D] = size_hi;
                s[0x10] = 1; // device_locator = string 1
                s[0x12] = mem_type;
                s[0x15] = speed_lo;
                s[0x16] = speed_hi;
                if use_ext {
                    let bytes = ext_size.to_le_bytes();
                    s[0x1C] = bytes[0];
                    s[0x1D] = bytes[1];
                    s[0x1E] = bytes[2];
                    s[0x1F] = bytes[3];
                }
                s.extend_from_slice(label.as_bytes());
                s.extend_from_slice(b"\0\0");
                s.extend_from_slice(&[127, 4, 0, 0, 0, 0]);

                let _ = parse_type17_entries(&s);
            }
        }
    }
}
