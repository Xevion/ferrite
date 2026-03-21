use std::fs;

use serde::Serialize;

/// SMBIOS Type 17 (Memory Device) information for a single DIMM slot.
#[derive(Debug, Clone, Serialize)]
pub struct DimmInfo {
    pub handle: u16,
    /// Silk-screen label, e.g., "DIMM_A1".
    pub device_locator: String,
    /// Bank grouping label, e.g., "BANK 0" or "P0_Node0_Channel0_Dimm0".
    pub bank_locator: String,
    pub manufacturer: Option<String>,
    pub serial_number: Option<String>,
    pub part_number: Option<String>,
    pub size_mb: u64,
    /// Memory type string, e.g., "DDR4" or "DDR5".
    pub memory_type: String,
    pub speed_mhz: u16,
}

/// Parse SMBIOS Type 17 entries from the raw DMI table.
/// Returns `None` if the DMI table cannot be read.
pub fn read_dimm_info() -> Option<Vec<DimmInfo>> {
    let table = fs::read("/sys/firmware/dmi/tables/DMI").ok()?;
    let dimms = parse_type17_entries(&table);
    if dimms.is_empty() { None } else { Some(dimms) }
}

/// Parse all Type 17 structures from a raw SMBIOS table blob.
fn parse_type17_entries(table: &[u8]) -> Vec<DimmInfo> {
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

            // Size: 0 = not installed, 0x7FFF = use extended size at 0x1C
            let size_mb = match size_raw {
                0 | 0xFFFF => 0,
                0x7FFF if struct_len >= 0x20 => {
                    let ext = u32::from_le_bytes([
                        table[offset + 0x1C],
                        table[offset + 0x1D],
                        table[offset + 0x1E],
                        table[offset + 0x1F],
                    ]);
                    ext as u64
                }
                other => {
                    // Bit 15: 0 = MB granularity, 1 = KB granularity
                    if other & 0x8000 != 0 {
                        (other & 0x7FFF) as u64 / 1024
                    } else {
                        other as u64
                    }
                }
            };

            // Skip empty slots
            if size_mb == 0 {
                offset = strings_end;
                continue;
            }

            dimms.push(DimmInfo {
                handle,
                device_locator: get_string(strings, device_locator_idx),
                bank_locator: get_string(strings, bank_locator_idx),
                manufacturer: non_empty(get_string(strings, manufacturer_idx)),
                serial_number: non_empty(get_string(strings, serial_idx)),
                part_number: non_empty(get_string(strings, part_idx)),
                size_mb,
                memory_type: memory_type_name(memory_type_byte),
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

/// Find the end of the string table (double NUL terminator).
fn find_string_table_end(table: &[u8], start: usize) -> usize {
    let mut i = start;
    // The string area is a sequence of NUL-terminated strings, terminated by an additional NUL.
    // So we look for \0\0.
    while i + 1 < table.len() {
        if table[i] == 0 && table[i + 1] == 0 {
            return i + 2;
        }
        i += 1;
    }
    table.len()
}

/// Get the Nth string (1-indexed) from the NUL-terminated string table.
fn get_string(strings: &[u8], index: u8) -> String {
    if index == 0 {
        return String::new();
    }
    let mut current = 1u8;
    let mut start = 0;
    while start < strings.len() {
        let end = strings[start..]
            .iter()
            .position(|&b| b == 0)
            .map(|p| start + p)
            .unwrap_or(strings.len());

        if current == index {
            return String::from_utf8_lossy(&strings[start..end])
                .trim()
                .to_owned();
        }
        current += 1;
        start = end + 1;
    }
    String::new()
}

fn non_empty(s: String) -> Option<String> {
    if s.is_empty() { None } else { Some(s) }
}

fn memory_type_name(byte: u8) -> String {
    match byte {
        0x01 => "Other",
        0x12 => "DDR",
        0x13 => "DDR2",
        0x18 => "DDR3",
        0x1A => "DDR4",
        0x22 => "DDR5",
        0x23 => "LPDDR5",
        _ => "Unknown",
    }
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_string_1_indexed() {
        let strings = b"Hello\0World\0Test\0";
        assert_eq!(get_string(strings, 1), "Hello");
        assert_eq!(get_string(strings, 2), "World");
        assert_eq!(get_string(strings, 3), "Test");
        assert_eq!(get_string(strings, 4), "");
        assert_eq!(get_string(strings, 0), "");
    }

    #[test]
    fn find_double_nul() {
        let data = b"abc\0def\0\0rest";
        assert_eq!(find_string_table_end(data, 0), 9);
    }

    #[test]
    fn parse_type17_fixture() {
        // Minimal Type 17 structure: 0x1B (27) bytes formatted area + string table
        let mut structure = vec![0u8; 0x1B];
        structure[0] = 17; // type
        structure[1] = 0x1B; // length
        structure[2] = 0x20; // handle low
        structure[3] = 0x00; // handle high
        // Bytes 0x0C-0x0D: size = 8192 MB
        structure[0x0C] = 0x00;
        structure[0x0D] = 0x20;
        // Byte 0x10: device locator = string 1
        structure[0x10] = 1;
        // Byte 0x11: bank locator = string 2
        structure[0x11] = 2;
        // Byte 0x12: memory type = DDR5
        structure[0x12] = 0x22;
        // Bytes 0x15-0x16: speed = 4800 MHz
        structure[0x15] = 0xC0;
        structure[0x16] = 0x12;
        // Byte 0x17: manufacturer = string 3
        structure[0x17] = 3;
        // Byte 0x18: serial = string 4
        structure[0x18] = 4;
        // Byte 0x1A: part number = string 5
        structure[0x1A] = 5;

        // String table
        let strings = b"DIMM_A1\0BANK 0\0Samsung\0SN12345\0M471A2G43AB2\0\0";
        structure.extend_from_slice(strings);

        // End-of-table marker
        structure.extend_from_slice(&[127, 4, 0xFF, 0xFF, 0, 0]);

        let dimms = parse_type17_entries(&structure);
        assert_eq!(dimms.len(), 1);
        let d = &dimms[0];
        assert_eq!(d.handle, 0x20);
        assert_eq!(d.device_locator, "DIMM_A1");
        assert_eq!(d.bank_locator, "BANK 0");
        assert_eq!(d.manufacturer.as_deref(), Some("Samsung"));
        assert_eq!(d.serial_number.as_deref(), Some("SN12345"));
        assert_eq!(d.part_number.as_deref(), Some("M471A2G43AB2"));
        assert_eq!(d.size_mb, 8192);
        assert_eq!(d.memory_type, "DDR5");
        assert_eq!(d.speed_mhz, 4800);
    }

    /// Build a Type 17 structure with the given size field and optional extended size,
    /// plus an end-of-table marker.
    fn build_type17(size_lo: u8, size_hi: u8, ext_size: Option<u32>) -> Vec<u8> {
        let struct_len = if ext_size.is_some() { 0x20 } else { 0x1B };
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
        // String table: "DIMM0\0\0"
        s.extend_from_slice(b"DIMM0\0\0");
        // End marker
        s.extend_from_slice(&[127, 4, 0, 0, 0, 0]);
        s
    }

    #[test]
    fn parse_kb_granularity_size() {
        // Bit 15 set = KB granularity, value 8192 KB = 8 MB
        // 0x8000 | 8192 = 0xA000
        let table = build_type17(0x00, 0xA0, None);
        let dimms = parse_type17_entries(&table);
        assert_eq!(dimms.len(), 1);
        assert_eq!(dimms[0].size_mb, 8); // 8192 KB / 1024 = 8 MB
    }

    #[test]
    fn parse_extended_size() {
        // size = 0x7FFF triggers extended size read at 0x1C
        let table = build_type17(0xFF, 0x7F, Some(32768));
        let dimms = parse_type17_entries(&table);
        assert_eq!(dimms.len(), 1);
        assert_eq!(dimms[0].size_mb, 32768);
    }

    #[test]
    fn parse_multiple_entries() {
        let struct_len = 0x1Bu8;
        let mut table = Vec::new();

        // First DIMM: 4096 MB
        let mut s1 = vec![0u8; 0x1B];
        s1[0] = 17;
        s1[1] = struct_len;
        s1[0x0C] = 0x00;
        s1[0x0D] = 0x10; // 4096 MB
        s1[0x10] = 1; // device_locator
        s1[0x12] = 0x1A; // DDR4
        table.extend_from_slice(&s1);
        table.extend_from_slice(b"SLOT1\0\0");

        // Second DIMM: 8192 MB
        let mut s2 = vec![0u8; 0x1B];
        s2[0] = 17;
        s2[1] = struct_len;
        s2[0x0C] = 0x00;
        s2[0x0D] = 0x20; // 8192 MB
        s2[0x10] = 1;
        s2[0x12] = 0x22; // DDR5
        table.extend_from_slice(&s2);
        table.extend_from_slice(b"SLOT2\0\0");

        // End marker
        table.extend_from_slice(&[127, 4, 0, 0, 0, 0]);

        let dimms = parse_type17_entries(&table);
        assert_eq!(dimms.len(), 2);
        assert_eq!(dimms[0].size_mb, 4096);
        assert_eq!(dimms[0].device_locator, "SLOT1");
        assert_eq!(dimms[0].memory_type, "DDR4");
        assert_eq!(dimms[1].size_mb, 8192);
        assert_eq!(dimms[1].memory_type, "DDR5");
    }

    #[test]
    fn parse_memory_type_variants() {
        assert_eq!(memory_type_name(0x01), "Other");
        assert_eq!(memory_type_name(0x12), "DDR");
        assert_eq!(memory_type_name(0x13), "DDR2");
        assert_eq!(memory_type_name(0x18), "DDR3");
        assert_eq!(memory_type_name(0x1A), "DDR4");
        assert_eq!(memory_type_name(0x22), "DDR5");
        assert_eq!(memory_type_name(0x23), "LPDDR5");
        assert_eq!(memory_type_name(0xFF), "Unknown");
    }

    #[test]
    fn parse_size_ffff_skipped() {
        // 0xFFFF means size is unknown/not reported — should be treated as 0
        let table = build_type17(0xFF, 0xFF, None);
        let dimms = parse_type17_entries(&table);
        assert!(dimms.is_empty());
    }

    #[test]
    fn parse_minimal_struct_length() {
        // Struct length exactly 0x17 — manufacturer/serial/part fields are absent
        let mut s = vec![0u8; 0x17];
        s[0] = 17;
        s[1] = 0x17;
        s[0x0C] = 0x00;
        s[0x0D] = 0x10; // 4096 MB
        s[0x10] = 1; // device_locator
        s[0x12] = 0x1A; // DDR4
        s[0x15] = 0xC0;
        s[0x16] = 0x12; // 4800 MHz
        s.extend_from_slice(b"DIMM0\0\0");
        s.extend_from_slice(&[127, 4, 0, 0, 0, 0]);

        let dimms = parse_type17_entries(&s);
        assert_eq!(dimms.len(), 1);
        assert!(dimms[0].manufacturer.is_none());
        assert!(dimms[0].serial_number.is_none());
        assert!(dimms[0].part_number.is_none());
    }

    #[test]
    fn get_string_out_of_range() {
        let strings = b"Hello\0World\0\0";
        // Index beyond available strings returns empty
        assert_eq!(get_string(strings, 10), "");
    }

    #[test]
    fn parse_empty_slot_skipped() {
        let mut structure = vec![0u8; 0x1B];
        structure[0] = 17;
        structure[1] = 0x1B;
        // size = 0 means not installed
        structure[0x0C] = 0;
        structure[0x0D] = 0;
        // String table (empty)
        structure.extend_from_slice(&[0, 0]);
        // End marker
        structure.extend_from_slice(&[127, 4, 0, 0, 0, 0]);

        let dimms = parse_type17_entries(&structure);
        assert!(dimms.is_empty());
    }
}
