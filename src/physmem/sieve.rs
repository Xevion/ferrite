//! Frame-hostage culling: actively steering allocation toward untested
//! physical frames (XEV-1018, `docs/COVERAGE.md` Tier 3).
//!
//! Userspace cannot ask the kernel for specific frames, but it can refuse to
//! give back frames it does not want re-served. Before the test buffer is
//! allocated, a [`FrameSieve`] sweeps available memory, resolves the physical
//! frames it received, and returns the *untested* ones to the kernel while
//! holding the previously-covered ones hostage (mapped and mlocked, so they
//! cannot be handed out again). The buddy allocator's LIFO reuse then serves
//! the just-released untested frames straight to the test buffer. Once the
//! test buffer is locked, the hostages are released.
//!
//! Culling granularity is one 2 MiB THP block: culling at page granularity
//! would split huge pages and crater fill/verify throughput.

use std::ffi::c_void;
use std::fs::File;
use std::num::NonZeroUsize;
use std::ptr::NonNull;

use nix::sys::mman::{MapFlags, ProtFlags, mmap_anonymous, munmap};
use snafu::{ResultExt, Snafu};

use crate::alloc::{AllocError, CHUNK_BYTES, MmapSnafu, activate_chunk, walk_chunks};
use crate::physmem::PAGE_BYTES_USIZE;
use crate::physmem::pfn::{Pfn, PfnRange, contains_pfn};

/// Failure modes for [`FrameSieve::hold`].
#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum SieveError {
    /// The sweep reservation could not be allocated.
    #[snafu(display("sieve reservation failed: {source}"))]
    #[snafu(context(false))]
    Alloc {
        /// Underlying allocation error.
        source: AllocError,
    },
    /// `/proc/self/pagemap` could not be opened or read to resolve the
    /// sweep's physical frames.
    #[snafu(display("sieve cannot resolve physical frames: {source}"))]
    Pagemap {
        /// Underlying I/O error.
        source: std::io::Error,
    },
}

/// Culling granularity: one transparent huge page.
pub const BLOCK_BYTES: usize = 2 * 1024 * 1024;

/// Frames per culling block.
pub const BLOCK_FRAMES: usize = BLOCK_BYTES / PAGE_BYTES_USIZE;

/// How a swept region splits into hostage and fresh block runs.
///
/// Offsets and lengths are in bytes relative to the region base. Runs of
/// adjacent same-disposition blocks are coalesced; the final block may be
/// shorter when the region is not block-aligned.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Partition {
    /// Block runs whose every frame is already covered -- keep mapped.
    pub hostages: Vec<(usize, usize)>,
    /// Block runs holding at least one untested or unresolved frame --
    /// return to the kernel for the test buffer to re-acquire.
    pub fresh: Vec<(usize, usize)>,
}

/// Split a swept region into hostage and fresh block runs.
///
/// `pfns` holds one entry per 4 KiB page in virtual order (0 = unresolved),
/// as produced by pagemap resolution. A block is a hostage only when every
/// frame in it resolved and is present in `covered`: holding a block with an
/// untested frame would hide that frame from the test buffer, which is worse
/// than re-testing a covered one.
pub(crate) fn partition_region(
    pfns: &[u64],
    covered: &[PfnRange],
    frames_per_block: usize,
) -> Partition {
    let mut partition = Partition {
        hostages: Vec::new(),
        fresh: Vec::new(),
    };
    for (block, frames) in pfns.chunks(frames_per_block).enumerate() {
        let hostage = frames
            .iter()
            .all(|&pfn| pfn != 0 && contains_pfn(covered, Pfn::new(pfn)));
        let offset = block * frames_per_block * PAGE_BYTES_USIZE;
        let len = frames.len() * PAGE_BYTES_USIZE;
        let runs = if hostage {
            &mut partition.hostages
        } else {
            &mut partition.fresh
        };
        match runs.last_mut() {
            Some((run_offset, run_len)) if *run_offset + *run_len == offset => *run_len += len,
            _ => runs.push((offset, len)),
        }
    }
    partition
}

/// Address-space holder for hostage blocks. Dropping it releases them.
pub struct FrameSieve {
    hostages: Vec<(NonNull<c_void>, usize)>,
}

// SAFETY: the hostage mappings are exclusively owned; the pointers are only
// used for munmap on drop.
unsafe impl Send for FrameSieve {}

/// What a sieve sweep accomplished, in bytes.
#[derive(Debug, Clone, Copy)]
pub struct SieveOutcome {
    /// Swept and resolved in total.
    pub swept: usize,
    /// Held hostage (previously covered).
    pub held: usize,
    /// Returned to the kernel for the test buffer to re-acquire.
    pub released: usize,
}

impl FrameSieve {
    /// Sweep available memory down to the `headroom` floor (at most
    /// `max_sweep` bytes when given), hold previously-covered 2 MiB blocks
    /// hostage, and return everything else to the kernel for the test buffer
    /// to re-acquire. Best-effort: a partial sweep still steers acquisition.
    ///
    /// The sweep is mlocked while held so that swap (where present) cannot
    /// evict hostages and recycle their frames into the test buffer.
    ///
    /// # Errors
    ///
    /// Returns [`SieveError`] when the reservation cannot be created or
    /// `/proc/self/pagemap` cannot be opened or read. A sweep stopped early
    /// by the headroom floor or a chunk failure is not an error.
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub fn hold(
        covered: &[PfnRange],
        headroom: u64,
        max_sweep: Option<usize>,
    ) -> Result<(Self, SieveOutcome), SieveError> {
        let empty = || {
            (
                Self {
                    hostages: Vec::new(),
                },
                SieveOutcome {
                    swept: 0,
                    held: 0,
                    released: 0,
                },
            )
        };

        let available = crate::physmem::sysmem::mem_available().unwrap_or(0);
        let mut target = usize::try_from(available.saturating_sub(headroom)).unwrap_or(usize::MAX);
        if let Some(max) = max_sweep {
            target = target.min(max);
        }
        target -= target % BLOCK_BYTES;
        let Some(size) = NonZeroUsize::new(target) else {
            return Ok(empty());
        };

        // Open pagemap before applying memory pressure so a doomed sweep
        // fails fast.
        let pagemap = File::open("/proc/self/pagemap").context(PagemapSnafu)?;

        // SAFETY: anonymous private reservation; no existing mapping replaced.
        let ptr = unsafe {
            mmap_anonymous(
                None,
                size,
                ProtFlags::PROT_NONE,
                MapFlags::MAP_PRIVATE | MapFlags::MAP_NORESERVE,
            )
            .context(MmapSnafu)?
        };
        let raw = ptr.as_ptr() as usize;

        let (achieved, _stop) = walk_chunks(
            target,
            CHUNK_BYTES,
            headroom,
            &mut || crate::physmem::sysmem::mem_available(),
            &mut |offset, len| activate_chunk(raw, offset, len),
        );

        if achieved == 0 {
            // SAFETY: unmapping the reservation we just created.
            unsafe {
                let _ = munmap(ptr, target);
            }
            return Ok(empty());
        }
        if achieved < target {
            // SAFETY: `raw` is a valid non-null mapping base and
            // `achieved < target`, so the sum cannot wrap to zero.
            let tail = unsafe { NonNull::new_unchecked((raw + achieved) as *mut c_void) };
            // SAFETY: [achieved, target) is the untouched reservation tail.
            unsafe {
                let _ = munmap(tail, target - achieved);
            }
        }

        let pfns = match crate::physmem::phys::read_pfns(&pagemap, raw, achieved / PAGE_BYTES_USIZE)
        {
            Ok(pfns) => pfns,
            Err(e) => {
                // Culling without resolution is useless: release everything.
                // SAFETY: [0, achieved) is our intact activated region.
                unsafe {
                    let _ = munmap(ptr, achieved);
                }
                return Err(SieveError::Pagemap { source: e });
            }
        };

        let partition = partition_region(&pfns, covered, BLOCK_FRAMES);

        let mut released = 0usize;
        for &(offset, len) in &partition.fresh {
            // SAFETY: fresh runs lie inside the activated region and are
            // disjoint from hostage runs; base is non-null so the sum cannot
            // wrap to zero.
            let run = unsafe { NonNull::new_unchecked((raw + offset) as *mut c_void) };
            // SAFETY: unmapping an exclusively-owned sub-range.
            unsafe {
                let _ = munmap(run, len);
            }
            released += len;
        }

        let mut held = 0usize;
        let hostages = partition
            .hostages
            .iter()
            .map(|&(offset, len)| {
                held += len;
                // SAFETY: hostage runs lie inside the activated region; base
                // is non-null so the sum cannot wrap to zero.
                (
                    unsafe { NonNull::new_unchecked((raw + offset) as *mut c_void) },
                    len,
                )
            })
            .collect();

        Ok((
            Self { hostages },
            SieveOutcome {
                swept: achieved,
                held,
                released,
            },
        ))
    }
}

impl Drop for FrameSieve {
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn drop(&mut self) {
        for &(ptr, len) in &self.hostages {
            // SAFETY: each hostage range is an intact, exclusively-owned
            // mapping produced by the sweep; fresh ranges and the tail were
            // already unmapped and are not in this list.
            unsafe {
                let _ = munmap(ptr, len);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    fn r(start: u64, count: u64) -> PfnRange {
        PfnRange {
            start: Pfn::new(start),
            count,
        }
    }

    /// 4 KiB frames per test block: 2 frames = 8192-byte blocks.
    const FPB: usize = 2;
    const BLOCK: usize = FPB * PAGE_BYTES_USIZE;

    #[test]
    fn empty_pfns_partition_to_nothing() {
        let p = partition_region(&[], &[r(0, 100)], FPB);
        check!(p.hostages == vec![]);
        check!(p.fresh == vec![]);
    }

    #[test]
    fn fully_covered_region_is_one_hostage_run() {
        // Frames 10..14 all inside covered [10, 20).
        let p = partition_region(&[10, 11, 12, 13], &[r(10, 10)], FPB);
        check!(p.hostages == vec![(0, 2 * BLOCK)]);
        check!(p.fresh == vec![]);
    }

    #[test]
    fn uncovered_region_is_one_fresh_run() {
        let p = partition_region(&[10, 11, 12, 13], &[r(100, 10)], FPB);
        check!(p.hostages == vec![]);
        check!(p.fresh == vec![(0, 2 * BLOCK)]);
    }

    #[test]
    fn alternating_blocks_split_with_correct_offsets() {
        // Block 0: frames 10,11 (covered). Block 1: frames 50,51 (not).
        // Block 2: frames 12,13 (covered).
        let pfns = [10, 11, 50, 51, 12, 13];
        let p = partition_region(&pfns, &[r(10, 4)], FPB);
        check!(p.hostages == vec![(0, BLOCK), (2 * BLOCK, BLOCK)]);
        check!(p.fresh == vec![(BLOCK, BLOCK)]);
    }

    #[test]
    fn adjacent_same_disposition_blocks_coalesce() {
        let pfns = [10, 11, 12, 13, 50, 51, 60, 61];
        let p = partition_region(&pfns, &[r(10, 4)], FPB);
        check!(p.hostages == vec![(0, 2 * BLOCK)]);
        check!(p.fresh == vec![(2 * BLOCK, 2 * BLOCK)]);
    }

    #[test]
    fn one_uncovered_frame_makes_the_block_fresh() {
        // Second frame of the block is outside the covered set.
        let p = partition_region(&[10, 99], &[r(10, 1)], FPB);
        check!(p.hostages == vec![]);
        check!(p.fresh == vec![(0, BLOCK)]);
    }

    #[test]
    fn unresolved_frame_makes_the_block_fresh() {
        // PFN 0 = unresolved: cannot prove coverage, so release it.
        let p = partition_region(&[10, 0], &[r(0, 100)], FPB);
        check!(p.hostages == vec![]);
        check!(p.fresh == vec![(0, BLOCK)]);
    }

    #[test]
    fn partial_last_block_has_short_length() {
        // 3 frames with 2-frame blocks: block 1 holds one 4096-byte frame.
        let p = partition_region(&[10, 11, 12], &[r(10, 3)], FPB);
        check!(p.hostages == vec![(0, BLOCK + PAGE_BYTES_USIZE)]);
        check!(p.fresh == vec![]);
    }

    #[test]
    fn partial_fresh_tail_after_hostage_run() {
        let p = partition_region(&[10, 11, 99], &[r(10, 2)], FPB);
        check!(p.hostages == vec![(0, BLOCK)]);
        check!(p.fresh == vec![(BLOCK, PAGE_BYTES_USIZE)]);
    }

    mod hold {
        use assert2::check;

        use super::*;

        #[test]
        fn bounded_sweep_with_no_coverage_holds_nothing() {
            // Without prior coverage every block is fresh. In environments
            // where mlock or the sweep fails, the sieve degrades to empty --
            // the invariants below hold either way.
            match FrameSieve::hold(&[], 0, Some(4 * BLOCK_BYTES)) {
                Ok((sieve, outcome)) => {
                    check!(outcome.held == 0);
                    check!(outcome.released == outcome.swept);
                    check!(outcome.swept <= 4 * BLOCK_BYTES);
                    drop(sieve);
                }
                Err(SieveError::Pagemap { .. }) => {
                    eprintln!("skipping: pagemap unavailable in this environment");
                }
                Err(e) => panic!("unexpected sieve error: {e}"),
            }
        }

        #[test]
        fn full_coverage_partitions_cleanly() {
            // Covering every possible PFN: with root everything swept is
            // held; without root frames resolve to 0 and are released.
            let covered = [r(1, u64::MAX - 1)];
            match FrameSieve::hold(&covered, 0, Some(2 * BLOCK_BYTES)) {
                Ok((sieve, outcome)) => {
                    check!(outcome.held + outcome.released == outcome.swept);
                    drop(sieve);
                }
                Err(SieveError::Pagemap { .. }) => {
                    eprintln!("skipping: pagemap unavailable in this environment");
                }
                Err(e) => panic!("unexpected sieve error: {e}"),
            }
        }

        #[test]
        fn zero_budget_is_an_empty_sieve() {
            let (sieve, outcome) = FrameSieve::hold(&[], u64::MAX, None).unwrap();
            check!(outcome.swept == 0);
            check!(outcome.held == 0);
            check!(outcome.released == 0);
            drop(sieve);
        }
    }
}
