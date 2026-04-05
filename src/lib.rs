pub mod alloc;
pub mod dimm;
pub mod edac;
pub mod error_analysis;
pub mod failure;
pub mod output;
pub mod pattern;
pub mod phys;
pub mod runner;
pub mod simd;
pub mod smbios;
#[cfg(feature = "tui")]
pub mod tui;
pub mod units;

pub use alloc::CompactionGuard;
pub use failure::Failure;

/// Internal functions exposed for benchmark targets via thin wrappers.
/// Not stable API — only available with `--features bench`.
#[cfg(feature = "bench")]
pub mod bench_api {
    use crate::{Failure, pattern, simd};

    pub fn scalar_fill_constant(buf: &mut [u64], pattern: u64) {
        pattern::scalar_fill_constant(buf, pattern);
    }

    pub fn scalar_fill_indexed(buf: &mut [u64], start: usize) {
        pattern::scalar_fill_indexed(buf, start);
    }

    #[must_use]
    pub fn scalar_verify_constant(
        buf: &[u64],
        pattern: u64,
        base_addr: usize,
        word_start: usize,
    ) -> Vec<Failure> {
        pattern::scalar_verify_constant(buf, pattern, base_addr, word_start)
    }

    #[must_use]
    pub fn scalar_verify_indexed(buf: &[u64], base_addr: usize, start: usize) -> Vec<Failure> {
        pattern::scalar_verify_indexed(buf, base_addr, start)
    }

    pub fn fill_verify_constant(
        buf: &mut [u64],
        pattern: u64,
        parallel: bool,
        on_activity: &(dyn Fn(f64) + Sync),
    ) -> Vec<Failure> {
        pattern::fill_verify_constant(buf, pattern, parallel, on_activity)
    }

    pub fn fill_verify_indexed(
        buf: &mut [u64],
        parallel: bool,
        on_activity: &(dyn Fn(f64) + Sync),
    ) -> Vec<Failure> {
        pattern::fill_verify_indexed(buf, parallel, on_activity)
    }

    #[cfg(target_arch = "x86_64")]
    #[must_use]
    pub fn avx512_available() -> bool {
        simd::avx512_available()
    }

    #[cfg(target_arch = "x86_64")]
    pub const CHUNK: usize = simd::CHUNK;

    /// # Safety
    ///
    /// Caller must verify AVX-512F is available via [`avx512_available`] before calling.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx512f")]
    pub unsafe fn fill_nt(buf: &mut [u64], pattern: u64) {
        // SAFETY: delegating to simd::fill_nt; caller guarantees AVX-512F is available.
        unsafe { simd::fill_nt(buf, pattern) }
    }

    /// # Safety
    ///
    /// Caller must verify AVX-512F is available via [`avx512_available`] before calling.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx512f")]
    pub unsafe fn fill_nt_indexed(buf: &mut [u64], start: usize) {
        // SAFETY: delegating to simd::fill_nt_indexed; caller guarantees AVX-512F is available.
        unsafe { simd::fill_nt_indexed(buf, start) }
    }

    /// # Safety
    ///
    /// Caller must verify AVX-512F is available via [`avx512_available`] before calling.
    #[cfg(target_arch = "x86_64")]
    #[must_use]
    #[target_feature(enable = "avx512f")]
    pub unsafe fn verify_avx512(
        buf: &[u64],
        pattern: u64,
        base_addr: usize,
        word_off: usize,
    ) -> Vec<Failure> {
        // SAFETY: delegating to simd::verify_avx512; caller guarantees AVX-512F is available.
        unsafe { simd::verify_avx512(buf, pattern, base_addr, word_off) }
    }

    /// # Safety
    ///
    /// Caller must verify AVX-512F is available via [`avx512_available`] before calling.
    #[cfg(target_arch = "x86_64")]
    #[must_use]
    #[target_feature(enable = "avx512f")]
    pub unsafe fn verify_indexed_avx512(
        buf: &[u64],
        base_addr: usize,
        word_off: usize,
    ) -> Vec<Failure> {
        // SAFETY: delegating to simd::verify_indexed_avx512; caller guarantees AVX-512F is available.
        unsafe { simd::verify_indexed_avx512(buf, base_addr, word_off) }
    }
}
