//! The public building blocks: each dispatches on the kernel's tier and store kind.

use snafu::Snafu;

use crate::caps::{Kernel, Stores, Tier};
use crate::fault::Faults;
use crate::kernels::{self, avx2, avx512, scalar, sse2};
use crate::prsg::Prsg;
use crate::region::Region;

/// Calls `$op` of the kernel's tier, with `NT` from its store kind where the op stores.
macro_rules! dispatch {
    ($kernel:expr, $op:ident, stores, ($($arg:expr),*)) => {{
        let nt = matches!($kernel.stores(), Stores::NonTemporal);
        // SAFETY: a Kernel exists only for a tier the CPU runs (Caps' contract), and the arguments
        // describe a Region, which the caller holds exclusively.
        unsafe {
            match ($kernel.tier(), nt) {
                (Tier::Scalar | Tier::RepString, _) => scalar::$op::<false>($($arg),*),
                (Tier::Sse2, false) => sse2::$op::<false>($($arg),*),
                (Tier::Sse2, true) => sse2::$op::<true>($($arg),*),
                (Tier::Avx2, false) => avx2::$op::<false>($($arg),*),
                (Tier::Avx2, true) => avx2::$op::<true>($($arg),*),
                (Tier::Avx512, false) => avx512::$op::<false>($($arg),*),
                (Tier::Avx512, true) => avx512::$op::<true>($($arg),*),
            }
        }
    }};
    ($kernel:expr, $op:ident, ($($arg:expr),*)) => {{
        // SAFETY: as above.
        unsafe {
            match $kernel.tier() {
                Tier::Scalar | Tier::RepString => scalar::$op($($arg),*),
                Tier::Sse2 => sse2::$op($($arg),*),
                Tier::Avx2 => avx2::$op($($arg),*),
                Tier::Avx512 => avx512::$op($($arg),*),
            }
        }
    }};
}

/// Store `value` in every word.
pub fn fill(kernel: Kernel, region: &mut Region<'_>, value: u64) {
    if kernel.tier() == Tier::RepString {
        // SAFETY: the region is exclusively ours.
        unsafe { kernels::rep_fill(region.ptr(), region.blocks(), value) };
        return;
    }
    dispatch!(kernel, fill_const, stores, (region.ptr(), region.blocks(), value));
}

/// Check that every word holds `value`.
pub fn verify(kernel: Kernel, region: &Region<'_>, value: u64, faults: &mut Faults<'_>) {
    dispatch!(kernel, verify_const, (region.ptr(), region.blocks(), value, faults));
}

/// Store in every word its own CPU address XOR `key` (`0` for the address, `!0` for its complement).
pub fn fill_address(kernel: Kernel, region: &mut Region<'_>, key: u64) {
    dispatch!(kernel, fill_address, stores, (region.ptr(), region.blocks(), key));
}

/// Check that every word holds its own address XOR `key`.
pub fn verify_address(kernel: Kernel, region: &Region<'_>, key: u64, faults: &mut Faults<'_>) {
    dispatch!(kernel, verify_address, (region.ptr(), region.blocks(), key, faults));
}

/// Store the sequence from `prsg`'s current block on, XOR `key`, and advance `prsg` past the region.
pub fn fill_prsg(kernel: Kernel, region: &mut Region<'_>, prsg: &mut Prsg, key: u64) {
    dispatch!(kernel, fill_prsg, stores, (region.ptr(), region.blocks(), &mut prsg.lanes, key));
}

/// Check the sequence from `prsg`'s current block on, XOR `key`, and advance `prsg` past the region.
pub fn verify_prsg(kernel: Kernel, region: &Region<'_>, prsg: &mut Prsg, key: u64, faults: &mut Faults<'_>) {
    dispatch!(kernel, verify_prsg, (region.ptr(), region.blocks(), &mut prsg.lanes, key, faults));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    Ascending,
    Descending,
}

/// One moving-inversions pass: every word should hold `expect` and is then overwritten with `write`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inversion {
    pub expect: u64,
    pub write: u64,
    pub order: Order,
}

/// Read, check and rewrite each 64-byte block in `step.order`. The block is the unit for every
/// tier: all eight words are read and checked, then all eight written.
pub fn invert(kernel: Kernel, region: &mut Region<'_>, step: Inversion, faults: &mut Faults<'_>) {
    let descending = step.order == Order::Descending;
    dispatch!(kernel, invert, stores, (region.ptr(), region.blocks(), step.expect, step.write, descending, faults));
}

#[derive(Debug, Snafu, PartialEq, Eq)]
#[snafu(display("copy from {src} words into {dst} words"))]
pub struct CopyError {
    src: usize,
    dst: usize,
}

/// Copy `src` into `dst`, ascending; the regions are disjoint by construction.
pub fn copy(kernel: Kernel, src: &Region<'_>, dst: &mut Region<'_>) -> Result<(), CopyError> {
    snafu::ensure!(src.words() == dst.words(), CopySnafu { src: src.words(), dst: dst.words() });
    if kernel.tier() == Tier::RepString {
        // SAFETY: both regions are ours and of equal length.
        unsafe { kernels::rep_copy(src.ptr(), dst.ptr(), src.blocks()) };
        return Ok(());
    }
    dispatch!(kernel, copy, stores, (src.ptr(), dst.ptr(), src.blocks()));
    Ok(())
}
