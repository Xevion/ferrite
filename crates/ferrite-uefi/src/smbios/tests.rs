use alloc::string::ToString as _;
use alloc::vec;
use alloc::vec::Vec;

use assert2::{assert, check};
use rstest::rstest;

use super::*;

/// A structure: its header, `body` from offset 4 on zero-filled to `len` bytes, then the strings.
fn structure(kind: u8, handle: u16, len: usize, fields: &[(usize, &[u8])], strings: &[&str]) -> Vec<u8> {
    let mut s = vec![0_u8; len];
    s[0] = kind;
    s[1] = u8::try_from(len).unwrap();
    s[2..4].copy_from_slice(&handle.to_le_bytes());
    for &(offset, bytes) in fields {
        s[offset..offset + bytes.len()].copy_from_slice(bytes);
    }
    for string in strings {
        s.extend_from_slice(string.as_bytes());
        s.push(0);
    }
    if strings.is_empty() {
        s.push(0);
    }
    s.push(0);
    s
}

const END: [u8; 6] = [127, 4, 0xff, 0xff, 0, 0];

/// A memory device laid out as SMBIOS 3.3 (0x5c bytes) with the fields given.
fn device(handle: u16, fields: &[(usize, &[u8])], strings: &[&str]) -> Vec<u8> {
    structure(17, handle, 0x5c, fields, strings)
}

/// An 8 GiB LPDDR3-2133 module of a soldered-down pair, as the X1 Carbon's table reads: a 64-bit bus
/// with no check bits, one rank, running at its rated speed.
fn lpddr3(handle: u16, locator: &str) -> Vec<u8> {
    device(
        handle,
        &[
            (0x04, &0x1000_u16.to_le_bytes()),
            (0x08, &64_u16.to_le_bytes()),
            (0x0a, &64_u16.to_le_bytes()),
            (0x0c, &8192_u16.to_le_bytes()),
            (0x0e, &[0x0b]),
            (0x10, &[1]),
            (0x11, &[2]),
            (0x12, &[0x1d]),
            (0x15, &2133_u16.to_le_bytes()),
            (0x17, &[3]),
            (0x1a, &[4]),
            (0x1b, &[1]),
            (0x20, &2133_u16.to_le_bytes()),
        ],
        &[locator, "BANK 0", "Samsung", "K4EBE304EB-EGCF   "],
    )
}

fn array(handle: u16, correction: u8, slots: u16) -> Vec<u8> {
    structure(16, handle, 0x17, &[(0x05, &[3]), (0x06, &[correction]), (0x0d, &slots.to_le_bytes())], &[])
}

fn table(parts: &[Vec<u8>]) -> Vec<u8> {
    let mut t: Vec<u8> = parts.concat();
    t.extend(END);
    t
}

#[test]
fn a_soldered_lpddr3_pair_reads_as_two_modules() {
    let memory = Memory::parse(&table(&[
        array(0x1000, 3, 2),
        lpddr3(0x1100, "ChannelA-DIMM0"),
        lpddr3(0x1101, "ChannelB-DIMM0"),
    ]));
    check!(memory.devices.len() == 2);
    let d = &memory.devices[1];
    check!(d.handle == Handle(0x1101));
    check!(d.array == Handle(0x1000));
    check!(d.locator.as_deref() == Some("ChannelB-DIMM0"));
    check!(d.bank.as_deref() == Some("BANK 0"));
    check!(d.size == Size::Bytes(NonZeroU64::new(8 << 30).unwrap()));
    check!(d.kind == MemoryKind::Lpddr3);
    check!(d.form_factor == FormFactor::RowOfChips);
    check!(d.speed == MegaTransfers::new(2133));
    check!(d.configured_speed == MegaTransfers::new(2133));
    check!(d.manufacturer.as_deref() == Some("Samsung"));
    check!(d.part_number.as_deref() == Some("K4EBE304EB-EGCF"), "the padding is trimmed");
    check!(d.ranks == Some(1));
    check!((d.data_width, d.total_width) == (Some(64), Some(64)));
    check!(d.has_check_bits() == Some(false));
    check!(d.size.to_string() == "8 GiB");
}

#[rstest]
#[case::empty_slot(0, None, Size::Empty)]
#[case::unknown(0xffff, None, Size::Unknown)]
#[case::megabytes(8192, None, Size::Bytes(NonZeroU64::new(8 << 30).unwrap()))]
#[case::the_largest_plain_size(0x7ffe, None, Size::Bytes(NonZeroU64::new(0x7ffe << 20).unwrap()))]
#[case::kilobytes(0x8000 | 512, None, Size::Bytes(NonZeroU64::new(512 << 10).unwrap()))]
#[case::zero_kilobytes(0x8000, None, Size::Empty)]
#[case::extended(0x7fff, Some(65_536), Size::Bytes(NonZeroU64::new(64 << 30).unwrap()))]
#[case::extended_ignores_the_reserved_bit(0x7fff, Some(0x8000_0000 | 32_768), Size::Bytes(NonZeroU64::new(32 << 30).unwrap()))]
#[case::extended_is_missing(0x7fff, None, Size::Unknown)]
#[case::extended_says_nothing(0x7fff, Some(0), Size::Unknown)]
#[case::extended_unused_for_a_plain_size(8192, Some(65_536), Size::Bytes(NonZeroU64::new(8 << 30).unwrap()))]
fn the_size_field_has_units_an_escape_and_two_empty_values(
    #[case] raw: u16,
    #[case] extended: Option<u32>,
    #[case] size: Size,
) {
    check!(Size::decode(raw, extended) == size);
}

#[test]
fn sizes_are_written_in_the_largest_whole_unit() {
    let size = |bytes: u64| Size::Bytes(NonZeroU64::new(bytes).unwrap()).to_string();
    check!(size(16 << 30) == "16 GiB");
    check!(size(1536 << 20) == "1536 MiB");
    check!(size(512 << 10) == "512 KiB");
    check!(size(3) == "3 B");
    check!(Size::Empty.to_string() == "empty");
}

#[rstest]
#[case::plain(2133, None, Some(2133))]
#[case::unknown(0, None, None)]
#[case::extended(0xffff, Some(6400), Some(6400))]
#[case::extended_with_the_reserved_bit(0xffff, Some(0x8000_20d0), Some(8400))]
#[case::extended_is_missing(0xffff, None, None)]
#[case::extended_is_zero(0xffff, Some(0), None)]
fn speeds_follow_the_extended_field_when_the_plain_one_is_full(
    #[case] raw: u16,
    #[case] extended: Option<u32>,
    #[case] speed: Option<u32>,
) {
    check!(MegaTransfers::decode(raw, extended).map(MegaTransfers::get) == speed);
}

#[test]
fn a_ddr5_module_uses_every_extended_field() {
    let memory = Memory::parse(&table(&[device(
        0x20,
        &[
            (0x04, &0x10_u16.to_le_bytes()),
            (0x08, &80_u16.to_le_bytes()),
            (0x0a, &64_u16.to_le_bytes()),
            (0x0c, &0x7fff_u16.to_le_bytes()),
            (0x0e, &[0x09]),
            (0x12, &[0x22]),
            (0x15, &0xffff_u16.to_le_bytes()),
            (0x1b, &[0x82]),
            (0x1c, &65_536_u32.to_le_bytes()),
            (0x20, &0xffff_u16.to_le_bytes()),
            (0x54, &8400_u32.to_le_bytes()),
            (0x58, &7200_u32.to_le_bytes()),
        ],
        &[],
    )]));
    let d = &memory.devices[0];
    check!(d.size == Size::Bytes(NonZeroU64::new(64 << 30).unwrap()));
    check!(d.kind == MemoryKind::Ddr5);
    check!(d.form_factor == FormFactor::Dimm);
    check!(d.speed.map(MegaTransfers::get) == Some(8400));
    check!(d.configured_speed.map(MegaTransfers::get) == Some(7200));
    check!(d.ranks == Some(2), "only the low nibble of the attributes is the rank");
    check!(d.has_check_bits() == Some(true), "80 bits on a 64 bit bus");
    check!(d.locator.is_none());
}

#[test]
fn a_structure_from_an_old_specification_has_no_newer_fields() {
    // SMBIOS 2.1 ends at 0x15: no speed, no part number, no rank.
    let memory = Memory::parse(&table(&[structure(
        17,
        0x30,
        0x15,
        &[(0x04, &1_u16.to_le_bytes()), (0x0c, &1024_u16.to_le_bytes()), (0x12, &[0x13])],
        &[],
    )]));
    let d = &memory.devices[0];
    check!(d.kind == MemoryKind::Ddr2);
    check!(d.size == Size::Bytes(NonZeroU64::new(1 << 30).unwrap()));
    check!((d.speed, d.configured_speed, d.ranks, d.part_number.clone()) == (None, None, None, None));
    // Shorter than that is not a memory device.
    check!(Memory::parse(&table(&[structure(17, 0x31, 0x10, &[], &[])])).devices.is_empty());
}

#[rstest]
#[case::both(64, 72, Some(true))]
#[case::none(64, 64, Some(false))]
#[case::unknown_total(64, 0xffff, None)]
#[case::no_width(0, 0, None)]
fn check_bits_are_a_total_width_above_the_data_width(
    #[case] data: u16,
    #[case] total: u16,
    #[case] want: Option<bool>,
) {
    let memory = Memory::parse(&table(&[device(
        1,
        &[
            (0x04, &1_u16.to_le_bytes()),
            (0x08, &total.to_le_bytes()),
            (0x0a, &data.to_le_bytes()),
            (0x0c, &4096_u16.to_le_bytes()),
        ],
        &[],
    )]));
    check!(memory.devices[0].has_check_bits() == want);
}

#[rstest]
#[case(0x03, ErrorCorrection::None, Some(false))]
#[case(0x04, ErrorCorrection::Parity, Some(false))]
#[case(0x05, ErrorCorrection::SingleBitEcc, Some(true))]
#[case(0x06, ErrorCorrection::MultiBitEcc, Some(true))]
#[case(0x07, ErrorCorrection::Crc, Some(true))]
#[case(0x02, ErrorCorrection::Unspecified, None)]
#[case(0x01, ErrorCorrection::Other, None)]
#[case(0x42, ErrorCorrection::Unknown(0x42), None)]
fn the_array_says_what_error_correction_it_has(
    #[case] raw: u8,
    #[case] correction: ErrorCorrection,
    #[case] corrects: Option<bool>,
) {
    let memory = Memory::parse(&table(&[array(0x40, raw, 4)]));
    let a = memory.main_array().unwrap();
    check!(a.correction == correction);
    check!(a.correction.corrects() == corrects);
    check!((a.handle, a.slots, a.system) == (Handle(0x40), 4, true));
}

#[test]
fn the_main_array_is_the_one_marked_system_memory() {
    let mut video = array(0x41, 3, 1);
    video[0x05] = 0x04;
    let memory = Memory::parse(&table(&[video, array(0x42, 5, 2)]));
    check!(memory.main_array().map(|a| a.handle) == Some(Handle(0x42)));
}

#[test]
fn array_and_device_ranges_are_kilobytes_or_extended_bytes() {
    let kb = |kb: u32| kb.to_le_bytes();
    let array_range = |handle: u16, start: u32, end: u32, ext: Option<(u64, u64)>| {
        let mut fields =
            vec![(0x04, kb(start).to_vec()), (0x08, kb(end).to_vec()), (0x0c, 0x1000_u16.to_le_bytes().to_vec())];
        if let Some((a, b)) = ext {
            fields.push((0x0f, a.to_le_bytes().to_vec()));
            fields.push((0x17, b.to_le_bytes().to_vec()));
        }
        let fields: Vec<(usize, &[u8])> = fields.iter().map(|(o, v)| (*o, v.as_slice())).collect();
        structure(19, handle, 0x1f, &fields, &[])
    };
    let device_range = |device: u16, start: u32, end: u32, ext: Option<(u64, u64)>, position: u8, depth: u8| {
        let mut fields = vec![
            (0x04, kb(start).to_vec()),
            (0x08, kb(end).to_vec()),
            (0x0c, device.to_le_bytes().to_vec()),
            (0x0e, 0x1300_u16.to_le_bytes().to_vec()),
            (0x11, vec![position]),
            (0x12, vec![depth]),
        ];
        if let Some((a, b)) = ext {
            fields.push((0x13, a.to_le_bytes().to_vec()));
            fields.push((0x1b, b.to_le_bytes().to_vec()));
        }
        let fields: Vec<(usize, &[u8])> = fields.iter().map(|(o, v)| (*o, v.as_slice())).collect();
        structure(20, 0x1400 + device, 0x23, &fields, &[])
    };
    let memory = Memory::parse(&table(&[
        array_range(0x1300, 0, 0x00ff_ffff, None),
        array_range(0x1301, u32::MAX, u32::MAX, Some((1 << 32, (1 << 32) + (4 << 30) - 1))),
        device_range(0x1100, 0, 0x007f_ffff, None, 1, 2),
        device_range(0x1101, u32::MAX, u32::MAX, Some((8 << 30, (16 << 30) - 1)), 0xff, 0),
        // An unmapped slot, and one whose range ends before it starts.
        device_range(0x1102, 0, 0, None, 0, 0),
        device_range(0x1103, 0x100, 0x80, None, 0, 0),
    ]));
    let (first, second) = (memory.array_ranges[0], memory.array_ranges[1]);
    check!(first.extent == Extent::new(PhysAddr::new(0), 16 << 30), "the end is the last kilobyte, inclusive");
    check!(first.array == Handle(0x1000));
    check!(second.extent == Extent::new(PhysAddr::new(1 << 32), 4 << 30));
    check!(memory.device_ranges.len() == 2);
    let a = memory.range_of(Handle(0x1100)).unwrap();
    check!(a.extent == Extent::new(PhysAddr::new(0), 8 << 30));
    check!((a.position, a.depth, a.array_range) == (Some(1), Some(2), Handle(0x1300)));
    let b = memory.range_of(Handle(0x1101)).unwrap();
    check!(b.extent == Extent::new(PhysAddr::new(8 << 30), 8 << 30));
    check!((b.position, b.depth) == (None, Some(0)), "0xff is unknown, 0 is not interleaved");
    check!(memory.range_of(Handle(0x1102)).is_none());
}

#[test]
fn strings_are_by_index_and_cleaned() {
    let bytes = structure(1, 1, 8, &[], &["  first  ", "   ", "se\u{e9}c\u{1}ond"]);
    let s = Structures::new(&bytes).next().unwrap();
    check!(s.string(0) == None, "index 0 is no string");
    check!(s.string(1).as_deref() == Some("first"));
    check!(s.string(2) == None, "a string of padding is none");
    check!(s.string(3).as_deref() == Some("second"), "non-ASCII and control bytes are dropped");
    check!(s.string(4) == None, "past the set");
    let bare = structure(1, 1, 8, &[], &[]);
    check!(Structures::new(&bare).next().unwrap().string(1) == None);
}

#[test]
fn the_walk_steps_over_structures_it_does_not_read_and_stops_at_the_end_marker() {
    let mut t = table(&[structure(0, 0, 0x1a, &[], &["vendor", "version"]), array(1, 3, 1)]);
    t.extend(array(2, 3, 1));
    let kinds: Vec<u8> = Structures::new(&t).map(|s| s.kind).collect();
    check!(kinds == [0, 16], "nothing after the end marker is read");
}

#[test]
fn a_damaged_table_gives_fewer_facts_and_never_a_panic() {
    let good = table(&[
        array(0x1000, 5, 2),
        lpddr3(0x1100, "A"),
        lpddr3(0x1101, "B"),
        structure(19, 0x1300, 0x1f, &[(0x08, &0xffff_u32.to_le_bytes())], &[]),
        structure(20, 0x1400, 0x23, &[(0x0c, &0x1100_u16.to_le_bytes()), (0x08, &0xffff_u32.to_le_bytes())], &[]),
    ]);
    let whole = Memory::parse(&good);
    check!(whole.devices.len() == 2);
    for cut in 0..good.len() {
        let _ = Memory::parse(&good[..cut]);
    }
    for at in 0..good.len() {
        for byte in [0x00, 0x01, 0x04, 0x7f, 0xfe, 0xff] {
            let mut bad = good.clone();
            bad[at] = byte;
            let _ = Memory::parse(&bad);
        }
    }
    // A length longer than the table, one shorter than the header, and a string set with no end.
    check!(Structures::new(&[17, 0xff, 0, 0, 0, 0]).next().is_none());
    check!(Structures::new(&[17, 3, 0, 0, 0, 0]).next().is_none());
    check!(Structures::new(&[17, 4, 0, 0, b'a', b'b']).next().is_none());
    check!(Structures::new(&[]).next().is_none());
}

#[test]
fn a_table_cannot_grow_the_lists_past_the_cap() {
    let many: Vec<Vec<u8>> = (0..MAX_ENTRIES + 40).map(|i| lpddr3(u16::try_from(i).unwrap(), "x")).collect();
    assert!(Memory::parse(&table(&many)).devices.len() == MAX_ENTRIES);
}
