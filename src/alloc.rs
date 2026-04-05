use std::ffi::c_void;
use std::num::NonZeroUsize;
use std::ptr::NonNull;

use nix::sys::mman::{MapFlags, MmapAdvise, ProtFlags, madvise, mlock, mmap_anonymous, munmap};
use rayon::prelude::*;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AllocError {
    #[error("requested size must be non-zero")]
    ZeroSize,
    #[error("mmap failed: {0}")]
    Mmap(#[source] nix::Error),
    #[error("mlock failed (are you root or do you have CAP_IPC_LOCK?): {0}")]
    Mlock(#[source] nix::Error),
}

/// A region of anonymous memory that is mmap'd and mlock'd.
/// Automatically unmaps on drop.
pub struct LockedRegion {
    ptr: NonNull<c_void>,
    len: usize,
}

// SAFETY: The memory region is exclusively owned and not shared across threads
// without synchronization. Raw pointer access is confined to this struct.
unsafe impl Send for LockedRegion {}

impl LockedRegion {
    /// Allocate and lock `size` bytes of anonymous memory.
    /// Pages are faulted in via parallel volatile writes before returning.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError`] if the size is zero, mmap fails, or mlock fails.
    pub fn new(size: usize) -> Result<Self, AllocError> {
        let size = NonZeroUsize::new(size).ok_or(AllocError::ZeroSize)?;

        // SAFETY: We request anonymous private memory with read/write access.
        let ptr = unsafe {
            mmap_anonymous(
                None,
                size,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_PRIVATE,
            )
            .map_err(AllocError::Mmap)?
        };

        let len = size.get();

        // Hint the kernel to back this region with 2 MiB transparent huge pages.
        // Must be called before pages are faulted in so the kernel uses huge pages
        // at fault time rather than collapsing 4 KiB pages later via khugepaged.
        // Silently ignored if THP is disabled system-wide.
        #[cfg(target_os = "linux")]
        unsafe {
            let _ = madvise(ptr, len, MmapAdvise::MADV_HUGEPAGE);
        }

        // Fault every page in parallel to both (a) ensure physical RAM is backed
        // before mlock and (b) allow the kernel to assign pages to NUMA-local nodes
        // for each faulting thread simultaneously.
        let raw = ptr.as_ptr() as usize;
        let page_count = len / 4096;
        (0..page_count).into_par_iter().for_each(|i| {
            // SAFETY: offset is within [0, len), each page is touched exactly once.
            unsafe { ((raw + i * 4096) as *mut u8).write_volatile(0u8) };
        });

        // SAFETY: ptr is valid for len bytes. mlock pins pages in physical RAM.
        unsafe {
            mlock(ptr, len).map_err(AllocError::Mlock)?;
        }

        Ok(Self { ptr, len })
    }

    /// Returns the buffer as a mutable slice of u64 words.
    /// The returned length is `self.len / 8` (trailing bytes are excluded).
    pub fn as_u64_slice_mut(&mut self) -> &mut [u64] {
        let word_count = self.len / size_of::<u64>();
        // SAFETY: The allocation is aligned to page boundaries (4096), which satisfies
        // u64 alignment (8). word_count * 8 <= self.len, so all accesses are in bounds.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr().cast::<u64>(), word_count) }
    }

    /// Returns the buffer as a slice of u64 words.
    #[must_use]
    pub fn as_u64_slice(&self) -> &[u64] {
        let word_count = self.len / size_of::<u64>();
        // SAFETY: Same alignment and bounds reasoning as as_u64_slice_mut.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr() as *const u64, word_count) }
    }

    /// The base virtual address of the locked region.
    #[must_use]
    pub fn as_ptr(&self) -> usize {
        self.ptr.as_ptr() as usize
    }

    /// The size in bytes of the locked region.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Always returns `false` — the constructor rejects zero-size allocations.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        false
    }
}

impl Drop for LockedRegion {
    fn drop(&mut self) {
        // SAFETY: ptr and len were produced by a successful mmap call.
        // We unmap the entire region.
        unsafe {
            let _ = munmap(self.ptr, self.len);
        }
    }
}

use std::fs;

const SYSCTL_PATH: &str = "/proc/sys/vm/compact_unevictable_allowed";

/// RAII guard that disables memory compaction of unevictable (mlocked) pages.
///
/// Writes `0` to `/proc/sys/vm/compact_unevictable_allowed` on creation
/// and restores the original value on drop. This prevents the kernel from
/// migrating mlocked pages during the test, keeping physical addresses stable.
pub struct CompactionGuard {
    original: String,
    changed: bool,
}

impl CompactionGuard {
    /// Disable compaction of unevictable pages. Returns `None` if the sysctl
    /// cannot be read or written (not root, file missing, etc.).
    #[must_use]
    pub fn new() -> Option<Self> {
        let original = fs::read_to_string(SYSCTL_PATH).ok()?.trim().to_owned();
        if original == "0" {
            return Some(Self {
                original,
                changed: false,
            });
        }
        fs::write(SYSCTL_PATH, "0\n").ok()?;
        Some(Self {
            original,
            changed: true,
        })
    }
}

impl Drop for CompactionGuard {
    fn drop(&mut self) {
        if self.changed {
            let _ = fs::write(SYSCTL_PATH, format!("{}\n", self.original));
        }
    }
}
