use assert2::check;
use rstest::rstest;

use super::*;

#[test]
fn hwp_capabilities_decode_into_their_four_levels_and_the_request_pins_the_highest() {
    // Kaby Lake R i5-8350U: highest 36, guaranteed 17, efficient 8, lowest 4.
    let levels = HwpLevels::decode(0x0408_1124);
    check!(levels == HwpLevels { highest: 0x24, guaranteed: 0x11, efficient: 0x08, lowest: 0x04 });
    check!(levels.request_highest() == 0x2424, "min and max at the highest, desired 0, EPP 0");
}

#[rstest]
#[case::base_17(0x8008_1100, Some(17))]
#[case::status_16(0x0000_1000, Some(16))]
#[case::empty(0x0000_00ff, None)]
fn the_ratio_field_is_bits_15_to_8(#[case] raw: u64, #[case] ratio: Option<u8>) {
    check!(ratio_field(raw).map(Ratio::get) == ratio);
}

#[test]
fn a_perf_ctl_request_keeps_every_bit_outside_the_ratio() {
    let ratio = Ratio::new(36).unwrap();
    check!(perf_ctl(0x1_0000_10ff, ratio) == 0x1_0000_24ff);
    check!(ratio.mhz() == 3600);
}

#[rstest]
#[case::kaby_lake_r(6, 0x8e, true)]
#[case::sandy_bridge(6, 0x2a, true)]
#[case::nehalem(6, 0x1a, false)]
#[case::goldmont(6, 0x5c, false)]
#[case::pentium_4(0xf, 0x02, false)]
fn only_core_line_models_from_sandy_bridge_on_are_known(#[case] family: u32, #[case] model: u32, #[case] known: bool) {
    check!(known_model(family, model) == known);
}
