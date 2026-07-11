#[cfg(target_arch = "x86_64")]
pub(crate) mod avx512;
pub(crate) mod scalar;

#[cfg(target_arch = "x86_64")]
use avx512::avx512_available;

use crate::{Failure, FailureBudget};

/// Chunk granularity (in u64 words) shared by scalar, AVX-512, and march
/// orchestration: 64 Ki words = 512 KiB. Kept as a multiple of 8 so every
/// chunk boundary is 64-byte aligned, which AVX-512 NT stores and aligned
/// loads require.
pub(crate) const CHUNK_WORDS: usize = 64 * 1024;

/// Fill every word with `pattern`, then verify. Returns any mismatches, capped
/// at the shared [`FailureBudget`].
///
/// Dispatches to AVX-512 on supported hardware, scalar otherwise.
#[cfg_attr(coverage_nightly, coverage(off))]
pub(crate) fn fill_verify_constant(
    buf: &mut [u64],
    pattern: u64,
    parallel: bool,
    budget: &FailureBudget,
    on_activity: &(dyn Fn(f64) + Sync),
) -> Vec<Failure> {
    #[cfg(target_arch = "x86_64")]
    if avx512_available() {
        return avx512::fill_verify_constant(buf, pattern, parallel, budget, on_activity);
    }

    scalar::fill_verify_constant(buf, pattern, parallel, budget, on_activity)
}

/// Fill every word with its index, then verify. Returns any mismatches, capped
/// at the shared [`FailureBudget`].
///
/// Dispatches to AVX-512 on supported hardware, scalar otherwise.
#[cfg_attr(coverage_nightly, coverage(off))]
pub(crate) fn fill_verify_indexed(
    buf: &mut [u64],
    parallel: bool,
    budget: &FailureBudget,
    on_activity: &(dyn Fn(f64) + Sync),
) -> Vec<Failure> {
    #[cfg(target_arch = "x86_64")]
    if avx512_available() {
        return avx512::fill_verify_indexed(buf, parallel, budget, on_activity);
    }

    scalar::fill_verify_indexed(buf, parallel, budget, on_activity)
}
