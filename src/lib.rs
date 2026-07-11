//! A userspace RAM tester: allocates and `mlock`s physical memory, then runs a suite of
//! fill-verify [`pattern`]s over it to detect stuck bits, address-line faults, and coupling
//! errors.
//!
//! # Core modules
//!
//! - [`alloc`]: mmap + `mlock` the test region and page-fault it in.
//! - [`pattern`]: the test patterns themselves (solid, walking ones/zeros, checkerboard,
//!   stuck-address, march, moving inversions, random-fill) and their [`pattern::metadata`].
//! - [`runner`]: drives each pattern over the allocation, emitting [`events::RunEvent`]s and
//!   collecting [`Failure`]s.
//! - [`physmem`]: physical-address resolution, coverage tracking, and gap classification.
//! - [`error_analysis`]: classifies collected [`Failure`]s as stuck-bit vs. coupling faults.
//! - [`edac`] / [`dimm`] / [`smbios`]: ECC counters, DIMM identity, and topology, so a failure
//!   can be pinned to a real module.
//!
//! # Output model
//!
//! Three independent consumers subscribe to the same [`events::RunEvent`] stream: [`headless`]
//! (human text to stdout), [`ndjson`] (the stable `--format json` wire format), and, with the
//! `tui` feature, [`tui`] (the inline live view). [`results`] renders the post-run summary from
//! either a live run or a saved JSON document. See `docs/ARCHITECTURE.md` for the full data flow
//! and `docs/VOCABULARY.md` for domain terminology.
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
// Promote to an unconditional `warn` (or `deny`) once docs are backfilled.
#![cfg_attr(doc, warn(missing_docs))]
#![deny(
    rustdoc::broken_intra_doc_links,
    rustdoc::private_intra_doc_links,
    rustdoc::missing_crate_level_docs,
    rustdoc::bare_urls,
    rustdoc::invalid_codeblock_attributes,
    rustdoc::invalid_rust_codeblocks,
    rustdoc::invalid_html_tags,
    rustdoc::unescaped_backticks,
    rustdoc::redundant_explicit_links
)]

pub mod alloc;
pub mod dimm;
pub mod edac;
pub mod error_analysis;
pub mod events;
pub mod failure;
pub mod headless;
pub mod log_bridge;
pub mod ndjson;
pub mod ops;
pub mod pattern;
pub mod pause;
pub mod physmem;
pub mod results;
pub mod runner;
pub mod shutdown;
pub mod smbios;
#[cfg(feature = "tui")]
pub mod tui;
pub mod units;

pub use alloc::CompactionGuard;
pub use failure::{Failure, FailureBudget};

/// Internal functions exposed for benchmark targets via thin wrappers.
/// Not stable API -- only available with `--features bench`.
#[cfg(feature = "bench")]
pub mod bench_api {
    use crate::Failure;
    use crate::ops::{avx512, scalar};

    pub fn scalar_fill_constant(buf: &mut [u64], pattern: u64) {
        scalar::fill_constant(buf, pattern);
    }

    pub fn scalar_fill_indexed(buf: &mut [u64], start: usize) {
        scalar::fill_indexed(buf, start);
    }

    #[must_use]
    pub fn scalar_verify_constant(
        buf: &[u64],
        pattern: u64,
        base_addr: usize,
        word_start: usize,
    ) -> Vec<Failure> {
        scalar::verify_constant(buf, pattern, base_addr, word_start)
    }

    #[must_use]
    pub fn scalar_verify_indexed(buf: &[u64], base_addr: usize, start: usize) -> Vec<Failure> {
        scalar::verify_indexed(buf, base_addr, start)
    }

    pub fn fill_verify_constant(
        buf: &mut [u64],
        pattern: u64,
        parallel: bool,
        on_activity: &(dyn Fn(f64) + Sync),
    ) -> Vec<Failure> {
        crate::ops::fill_verify_constant(
            buf,
            pattern,
            parallel,
            &crate::FailureBudget::unlimited(),
            on_activity,
        )
    }

    pub fn fill_verify_indexed(
        buf: &mut [u64],
        parallel: bool,
        on_activity: &(dyn Fn(f64) + Sync),
    ) -> Vec<Failure> {
        crate::ops::fill_verify_indexed(
            buf,
            parallel,
            &crate::FailureBudget::unlimited(),
            on_activity,
        )
    }

    #[cfg(target_arch = "x86_64")]
    #[cfg_attr(coverage_nightly, coverage(off))]
    #[must_use]
    pub fn avx512_available() -> bool {
        avx512::avx512_available()
    }

    #[cfg(target_arch = "x86_64")]
    pub const CHUNK: usize = avx512::CHUNK;

    /// # Safety
    ///
    /// Caller must verify AVX-512F is available via [`avx512_available`] before calling.
    #[cfg(target_arch = "x86_64")]
    #[cfg_attr(coverage_nightly, coverage(off))]
    #[target_feature(enable = "avx512f")]
    pub unsafe fn fill_nt(buf: &mut [u64], pattern: u64) {
        // SAFETY: delegating to avx512::fill_nt; caller guarantees AVX-512F is available.
        unsafe { avx512::fill_nt(buf, pattern) }
    }

    /// # Safety
    ///
    /// Caller must verify AVX-512F is available via [`avx512_available`] before calling.
    #[cfg(target_arch = "x86_64")]
    #[cfg_attr(coverage_nightly, coverage(off))]
    #[target_feature(enable = "avx512f")]
    pub unsafe fn fill_nt_indexed(buf: &mut [u64], start: usize) {
        // SAFETY: delegating to avx512::fill_nt_indexed; caller guarantees AVX-512F is available.
        unsafe { avx512::fill_nt_indexed(buf, start) }
    }

    /// # Safety
    ///
    /// Caller must verify AVX-512F is available via [`avx512_available`] before calling.
    #[cfg(target_arch = "x86_64")]
    #[cfg_attr(coverage_nightly, coverage(off))]
    #[must_use]
    #[target_feature(enable = "avx512f")]
    pub unsafe fn verify_avx512(
        buf: &[u64],
        pattern: u64,
        base_addr: usize,
        word_off: usize,
    ) -> Vec<Failure> {
        // SAFETY: delegating to avx512::verify_avx512; caller guarantees AVX-512F is available.
        unsafe { avx512::verify_avx512(buf, pattern, base_addr, word_off) }
    }

    /// # Safety
    ///
    /// Caller must verify AVX-512F is available via [`avx512_available`] before calling.
    #[cfg(target_arch = "x86_64")]
    #[cfg_attr(coverage_nightly, coverage(off))]
    #[must_use]
    #[target_feature(enable = "avx512f")]
    pub unsafe fn verify_indexed_avx512(
        buf: &[u64],
        base_addr: usize,
        word_off: usize,
    ) -> Vec<Failure> {
        // SAFETY: delegating to avx512::verify_indexed_avx512; caller guarantees AVX-512F is available.
        unsafe { avx512::verify_indexed_avx512(buf, base_addr, word_off) }
    }
}
