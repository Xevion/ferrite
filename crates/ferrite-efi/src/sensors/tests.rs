use assert2::check;
use rstest::rstest;

use super::*;

fn intel(model: u8) -> Signature {
    Signature { family: 6, model, stepping: 0 }
}

#[test]
fn the_signature_folds_in_the_extended_fields() {
    // Kaby Lake R: family 6, extended model 8, model 0xe, stepping 10.
    check!(Signature::from_eax(0x0008_06ea) == Signature { family: 6, model: 0x8e, stepping: 10 });
    // Zen 3: base family 0xf and extended family 0xa make family 25; extended model 2, model 1.
    check!(Signature::from_eax(0x00a2_0f10) == Signature { family: 25, model: 0x21, stepping: 0 });
}

#[test]
fn the_vendor_string_is_read_in_the_registers_order() {
    let le = |s: &[u8; 4]| u32::from_le_bytes(*s);
    check!(Vendor::from_registers(le(b"Genu"), le(b"ntel"), le(b"ineI")) == Vendor::Intel);
    check!(Vendor::from_registers(le(b"Auth"), le(b"cAMD"), le(b"enti")) == Vendor::Amd);
    assert2::assert!(let Vendor::Other(_) = Vendor::from_registers(0, 0, 0));
}

#[test]
fn the_brand_string_is_trimmed_and_cut_at_the_nul() {
    let words = |text: &[u8]| {
        let mut bytes = [0_u8; 48];
        bytes[..text.len()].copy_from_slice(text);
        let mut leaves = [[0_u32; 4]; 3];
        for (i, word) in bytes.as_chunks::<4>().0.iter().enumerate() {
            leaves[i / 4][i % 4] = u32::from_le_bytes(*word);
        }
        leaves
    };
    check!(
        brand_string(&words(b"      Intel(R) Core(TM) i7-8650U CPU @ 1.90GHz"))
            == "Intel(R) Core(TM) i7-8650U CPU @ 1.90GHz"
    );
    check!(brand_string(&[[0; 4]; 3]) == "");
}

#[test]
fn the_cpuid_bits_say_which_registers_exist() {
    // Kaby Lake R leaf 6: digital sensor, package thermal, APERF/MPERF; leaf 0x16 base 1900.
    let f = Features::from_leaves(0x7ffa_fbff, Some((0x0000_27f7, 0x0000_0009)), Some(1900));
    check!(f.digital_sensor && f.package_sensor && f.aperf_mperf && !f.hypervisor);
    check!(f.base_mhz == Some(1900));
    let vm = Features::from_leaves(1 << 31, None, Some(0));
    check!(vm.hypervisor && !vm.digital_sensor && !vm.aperf_mperf && vm.base_mhz.is_none());
}

#[rstest]
#[case::kaby_lake_r(Vendor::Intel, intel(0x8e), true, true, Ok(Plan::IntelDigital { package: true }))]
#[case::no_package_sensor(Vendor::Intel, intel(0x3c), true, false, Ok(Plan::IntelDigital { package: false }))]
#[case::no_digital_sensor(Vendor::Intel, intel(0x8e), false, false, Err("CPUID reports no digital thermal sensor"))]
#[case::first_atom(Vendor::Intel, intel(0x1c), true, false, Err("no TjMax register on this model"))]
#[case::zen(Vendor::Amd, Signature { family: 0x17, model: 0x71, stepping: 0 }, true, false, Ok(Plan::AmdSmn))]
#[case::before_zen(
    Vendor::Amd,
    Signature { family: 0x15, model: 1, stepping: 0 },
    true,
    false,
    Err("no SMN thermal register before AMD family 17h")
)]
fn the_temperature_plan_follows_the_cpu(
    #[case] vendor: Vendor,
    #[case] signature: Signature,
    #[case] digital_sensor: bool,
    #[case] package_sensor: bool,
    #[case] want: Result<Plan, &'static str>,
) {
    let features = Features { hypervisor: false, digital_sensor, package_sensor, aperf_mperf: true, base_mhz: None };
    check!(plan(vendor, signature, &features) == want);
}

#[test]
fn a_hypervisor_gets_no_register_reads() {
    let features =
        Features { hypervisor: true, digital_sensor: true, package_sensor: true, aperf_mperf: true, base_mhz: None };
    check!(plan(Vendor::Intel, intel(0x8e), &features) == Err("running under a hypervisor"));
}

#[test]
fn the_digital_readout_counts_down_from_tjmax() {
    // TjMax is in bits 23:16 of MSR_TEMPERATURE_TARGET.
    check!(tj_max(0x0064_0000) == Some(100));
    check!(tj_max(0) == None);
    // Valid, 42 degrees below TjMax.
    check!(digital_celsius(0x802a_0000, 100) == Some(58));
    // The same readout with the valid bit clear is no reading.
    check!(digital_celsius(0x002a_0000, 100) == None);
}

#[rstest]
#[case::room(200_u32 << 21, Some(25))]
#[case::rounds_to_nearest(201_u32 << 21, Some(25))]
#[case::range_select_lowers_it(0x0008_0000 | (800_u32 << 21), Some(51))]
#[case::below_zero(0x0008_0000 | (160_u32 << 21), Some(-29))]
#[case::nothing_there(u32::MAX, None)]
fn the_amd_register_is_eighths_of_a_degree_with_a_range_bit(#[case] raw: u32, #[case] celsius: Option<i16>) {
    check!(smn_celsius(raw) == celsius);
}

#[rstest]
#[case::turbo(1_896_000_000, 3_000_000, 1_000_000, Some(5688))]
#[case::half_speed(1_896_000_000, 500_000, 1_000_000, Some(948))]
#[case::never_ran(1_896_000_000, 5, 0, None)]
#[case::no_progress(1_896_000_000, 0, 1_000_000, None)]
fn the_effective_clock_scales_the_tsc_by_the_counters(
    #[case] tsc_hz: u64,
    #[case] aperf: u64,
    #[case] mperf: u64,
    #[case] mhz: Option<u32>,
) {
    check!(effective_mhz(tsc_hz, aperf, mperf) == mhz);
}
