#[cfg(target_arch = "x86_64")]
pub(crate) mod avx512;
pub(crate) mod scalar;

#[cfg(target_arch = "x86_64")]
use avx512::avx512_available;

use crate::Failure;

/// Fill every word with `pattern`, then verify. Returns any mismatches.
///
/// Dispatches to AVX-512 on supported hardware, scalar otherwise.
#[cfg_attr(coverage_nightly, coverage(off))]
pub(crate) fn fill_verify_constant(
    buf: &mut [u64],
    pattern: u64,
    parallel: bool,
    on_activity: &(dyn Fn(f64) + Sync),
) -> Vec<Failure> {
    #[cfg(target_arch = "x86_64")]
    if avx512_available() {
        return avx512::fill_verify_constant(buf, pattern, parallel, on_activity);
    }

    scalar::fill_verify_constant(buf, pattern, parallel, on_activity)
}

/// Fill every word with its index, then verify. Returns any mismatches.
///
/// Dispatches to AVX-512 on supported hardware, scalar otherwise.
#[cfg_attr(coverage_nightly, coverage(off))]
pub(crate) fn fill_verify_indexed(
    buf: &mut [u64],
    parallel: bool,
    on_activity: &(dyn Fn(f64) + Sync),
) -> Vec<Failure> {
    #[cfg(target_arch = "x86_64")]
    if avx512_available() {
        return avx512::fill_verify_indexed(buf, parallel, on_activity);
    }

    scalar::fill_verify_indexed(buf, parallel, on_activity)
}
