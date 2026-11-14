use alloc::string::ToString as _;
use alloc::vec;
use alloc::vec::Vec;
use core::num::NonZeroU64;

use assert2::check;
use ferrite_efi::sensors::{Frequency, Signature, Temperature, Vendor};
use ferrite_hal::PhysAddr;
use rstest::rstest;

use super::*;
use crate::smbios::{DeviceRange, ErrorCorrection, FormFactor, Handle, MegaTransfers, MemoryArray, MemoryKind};

fn module(handle: u16, locator: &str, gib: u64, kind: MemoryKind, speed: u32) -> MemoryDevice {
    MemoryDevice {
        handle: Handle(handle),
        array: Handle(1),
        locator: Some(locator.into()),
        bank: None,
        size: NonZeroU64::new(gib << 30).map_or(Size::Empty, Size::Bytes),
        kind,
        form_factor: FormFactor::Dimm,
        speed: MegaTransfers::new(speed),
        configured_speed: None,
        manufacturer: None,
        part_number: None,
        ranks: None,
        data_width: Some(64),
        total_width: Some(64),
    }
}

fn mapped(device: u16, start: u64, gib: u64) -> DeviceRange {
    DeviceRange {
        device: Handle(device),
        array_range: Handle(0x30),
        extent: Extent::new(PhysAddr::new(start), gib << 30),
        position: Some(0),
        depth: Some(0),
    }
}

fn memory(devices: Vec<MemoryDevice>, ranges: Vec<DeviceRange>) -> Memory {
    Memory { devices, device_ranges: ranges, ..Memory::default() }
}

#[rstest]
#[case::identical_modules_group(
    vec![module(1, "A", 8, MemoryKind::Lpddr3, 2133), module(2, "B", 8, MemoryKind::Lpddr3, 2133)],
    "2 x 8 GiB LPDDR3-2133"
)]
#[case::different_modules_do_not(
    vec![module(1, "A", 8, MemoryKind::Ddr4, 3200), module(2, "B", 4, MemoryKind::Ddr4, 2400), module(3, "C", 8, MemoryKind::Ddr4, 3200)],
    "2 x 8 GiB DDR4-3200 + 1 x 4 GiB DDR4-2400"
)]
#[case::empty_slots_count_apart(
    vec![module(1, "A", 16, MemoryKind::Ddr5, 4800), module(2, "B", 0, MemoryKind::Ddr5, 0)],
    "1 x 16 GiB DDR5-4800, 1 empty"
)]
#[case::only_empty_slots(vec![module(1, "A", 0, MemoryKind::Unspecified, 0)], "1 empty")]
#[case::no_speed(vec![module(1, "A", 8, MemoryKind::Ddr3, 0)], "1 x 8 GiB DDR3")]
#[case::no_technology(vec![module(1, "A", 8, MemoryKind::Unspecified, 0)], "1 x 8 GiB")]
#[case::no_modules(vec![], "")]
fn the_modules_read_as_one_line(#[case] devices: Vec<MemoryDevice>, #[case] want: &str) {
    check!(modules_text(&memory(devices, vec![])) == want);
}

#[test]
fn the_speed_it_runs_at_is_the_one_shown() {
    let mut slow = module(1, "A", 8, MemoryKind::Ddr4, 3200);
    slow.configured_speed = MegaTransfers::new(2666);
    check!(modules_text(&memory(vec![slow], vec![])) == "1 x 8 GiB DDR4-2666");
}

#[test]
fn modules_mapped_to_the_same_range_are_interleaved() {
    let devices = vec![module(1, "A", 8, MemoryKind::Lpddr3, 2133), module(2, "B", 8, MemoryKind::Lpddr3, 2133)];
    let layout = Layout::of(&memory(devices, vec![mapped(1, 0, 16), mapped(2, 0, 16)]));
    check!(layout == Layout::Interleaved { ways: 2 });
    check!(layout.caption().as_deref() == Some("2 channels interleaved"));
}

#[test]
fn modules_with_ranges_of_their_own_are_bands_named_by_slot() {
    let devices = vec![
        module(1, "ChannelA-DIMM0", 8, MemoryKind::Ddr4, 3200),
        module(2, "ChannelB-DIMM0", 8, MemoryKind::Ddr4, 3200),
    ];
    let layout = Layout::of(&memory(devices, vec![mapped(1, 0, 8), mapped(2, 8 << 30, 8)]));
    let Layout::Bands(bands) = &layout else { panic!("{layout:?}") };
    check!(bands.iter().map(|b| b.label.as_str()).collect::<Vec<_>>() == ["ChannelA-DIMM0", "ChannelB-DIMM0"]);
    check!(bands[1].extent == Extent::new(PhysAddr::new(8 << 30), 8 << 30));
    check!(layout.caption().as_deref() == Some("2 modules, separate ranges"));
}

#[test]
fn a_layout_needs_ranges_for_populated_modules() {
    let devices = || vec![module(1, "A", 8, MemoryKind::Ddr4, 3200), module(2, "B", 0, MemoryKind::Ddr4, 0)];
    check!(Layout::of(&memory(devices(), vec![])) == Layout::Unknown);
    // The empty slot's range does not count, and one module is not a layout.
    check!(Layout::of(&memory(devices(), vec![mapped(1, 0, 8), mapped(2, 0, 8)])) == Layout::Single);
    check!(Layout::Single.caption().is_none() && Layout::Unknown.caption().is_none());
}

#[rstest]
#[case("Intel(R) Core(TM) i7-8650U CPU @ 1.90GHz", "Core i7-8650U")]
#[case("Intel(R) Xeon(R) CPU E5-2690 v4 @ 2.60GHz", "Xeon E5-2690 v4")]
#[case("Intel(R) Celeron(R) N4020 CPU @ 1.10GHz", "Celeron N4020")]
#[case("AMD Ryzen 7 5800X 8-Core Processor", "Ryzen 7 5800X")]
#[case("AMD Ryzen 7 PRO 4750U with Radeon Graphics", "Ryzen 7 PRO 4750U")]
#[case("Unusual Chip", "Unusual Chip")]
#[case("Intel(R) CPU", "Intel(R) CPU")]
#[case("", "")]
fn the_brand_is_shortened_to_the_part_name(#[case] brand: &str, #[case] short: &str) {
    check!(short_brand(brand) == short);
}

#[test]
fn the_clock_is_shown_to_ten_megahertz_so_a_smaller_change_is_not_a_new_figure() {
    let reading = |mhz, measured| Reading {
        frequency: Some(Frequency { mhz, measured }),
        temperature: Some(Temperature { celsius: 61, source: TempSource::IntelPackage }),
    };
    check!(Vitals::of(reading(1896, true)).mhz == Some(1900));
    check!(Vitals::of(reading(1894, true)).mhz == Some(1890));
    check!(Vitals::of(reading(1895, true)).mhz == Some(1900));
    check!(Vitals::of(reading(1896, true)) == Vitals::of(reading(1904, true)), "both show 1900, so neither redraws");
    check!(Vitals::of(reading(1900, false)).measured == false);
    check!(Vitals::of(Reading::default()) == Vitals::default());
}

#[test]
fn the_facts_of_a_machine_with_ecc_memory() {
    let cpu = Identity {
        vendor: Vendor::Intel,
        signature: Signature { family: 6, model: 0x8e, stepping: 10 },
        brand: "Intel(R) Core(TM) i7-8650U CPU @ 1.90GHz".into(),
    };
    let mut m = memory(vec![module(1, "A", 8, MemoryKind::Ddr4, 2666)], vec![]);
    m.arrays.push(MemoryArray { handle: Handle(1), system: true, correction: ErrorCorrection::SingleBitEcc, slots: 1 });
    let facts = Facts::new(cpu, m);
    check!(facts.cpu_short == "Core i7-8650U");
    check!(facts.corrects_errors() == Some(true));
    check!(facts.ecc_claim().map(|c| c.to_string()).as_deref() == Some("single-bit ECC"));
    check!(Facts::new(facts.cpu, Memory::default()).corrects_errors() == None);
}
