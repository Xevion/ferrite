use assert2::check;
use rstest::rstest;

use super::*;

fn intel(model: u8) -> Signature {
    Signature { family: 6, model, stepping: 0 }
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

/// The registers of leaves 0x80000002 to 0x80000004 for a brand string, as CPUID returns them.
fn brand_leaves_of(text: &[u8]) -> [[u32; 4]; 3] {
    let mut bytes = [0_u8; 48];
    bytes[..text.len()].copy_from_slice(text);
    let mut leaves = [[0_u32; 4]; 3];
    for (i, word) in bytes.as_chunks::<4>().0.iter().enumerate() {
        leaves[i / 4][i % 4] = u32::from_le_bytes(*word);
    }
    leaves
}

#[test]
fn the_fingerprint_is_the_raw_brand_bytes_then_leaf_1_eax() {
    let brand = b"Intel(R) Core(TM) i7-8650U CPU @ 1.90GHz";
    let mut want = [0_u8; 52];
    want[..brand.len()].copy_from_slice(brand);
    want[48..].copy_from_slice(&[0xea, 0x06, 0x08, 0x00]);
    check!(fingerprint_of(&brand_leaves_of(brand), 0x0008_06ea) == want);
}

#[test]
fn a_processor_without_a_brand_string_fingerprints_zeros_and_its_signature() {
    let mut want = [0_u8; 52];
    want[48..].copy_from_slice(&0x0008_06eau32.to_le_bytes());
    check!(fingerprint_of(&[[0; 4]; 3], 0x0008_06ea) == want);
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
    let features = [(Feature::DigitalSensor, digital_sensor), (Feature::PackageThermal, package_sensor)]
        .into_iter()
        .filter_map(|(f, on)| on.then_some(f))
        .collect();
    check!(plan(vendor, signature, features) == want);
}

#[test]
fn a_hypervisor_gets_no_register_reads() {
    let features = Features::EMPTY.with(Feature::Hypervisor).with(Feature::DigitalSensor);
    check!(plan(Vendor::Intel, intel(0x8e), features) == Err("running under a hypervisor"));
}
