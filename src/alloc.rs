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

impl AllocError {
    /// Human-readable remediation hint for this error variant, if one exists.
    /// Callers may display this alongside the error message to guide the user.
    #[must_use]
    pub fn help(&self) -> Option<&'static str> {
        match self {
            AllocError::Mlock(_) => Some(
                "run as root, raise the mlock limit (ulimit -l unlimited), \
                or grant the capability: sudo setcap cap_ipc_lock+ep $(which ferrite)",
            ),
            AllocError::Mmap(_) | AllocError::ZeroSize => None,
        }
    }
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
    #[cfg_attr(coverage_nightly, coverage(off))]
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
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub fn as_u64_slice_mut(&mut self) -> &mut [u64] {
        let word_count = self.len / size_of::<u64>();
        // SAFETY: The allocation is aligned to page boundaries (4096), which satisfies
        // u64 alignment (8). word_count * 8 <= self.len, so all accesses are in bounds.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr().cast::<u64>(), word_count) }
    }

    /// Returns the buffer as a slice of u64 words.
    #[must_use]
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub fn as_u64_slice(&self) -> &[u64] {
        let word_count = self.len / size_of::<u64>();
        // SAFETY: Same alignment and bounds reasoning as as_u64_slice_mut.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr() as *const u64, word_count) }
    }

    /// The base virtual address of the locked region.
    #[must_use]
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub fn as_ptr(&self) -> usize {
        self.ptr.as_ptr() as usize
    }

    /// The size in bytes of the locked region.
    #[must_use]
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Always returns `false` -- the constructor rejects zero-size allocations.
    #[must_use]
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub fn is_empty(&self) -> bool {
        false
    }
}

impl Drop for LockedRegion {
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn drop(&mut self) {
        // SAFETY: ptr and len were produced by a successful mmap call.
        // We unmap the entire region.
        unsafe {
            let _ = munmap(self.ptr, self.len);
        }
    }
}

use std::fs;
use std::path::{Path, PathBuf};

const SYSCTL_PATH: &str = "/proc/sys/vm/compact_unevictable_allowed";

/// RAII guard that disables memory compaction of unevictable (mlocked) pages.
///
/// Writes `0` to `/proc/sys/vm/compact_unevictable_allowed` on creation
/// and restores the original value on drop. This prevents the kernel from
/// migrating mlocked pages during the test, keeping physical addresses stable.
pub struct CompactionGuard {
    path: PathBuf,
    original: String,
    changed: bool,
}

impl CompactionGuard {
    /// Disable compaction of unevictable pages. Returns `None` if the sysctl
    /// cannot be read or written (not root, file missing, etc.).
    #[must_use]
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub fn new() -> Option<Self> {
        Self::with_path(Path::new(SYSCTL_PATH))
    }

    /// Disable compaction via an arbitrary sysctl path.
    /// Useful for testing with a temporary file.
    #[must_use]
    pub(crate) fn with_path(path: &Path) -> Option<Self> {
        let original = fs::read_to_string(path).ok()?.trim().to_owned();
        if original == "0" {
            return Some(Self {
                path: path.to_owned(),
                original,
                changed: false,
            });
        }
        fs::write(path, "0\n").ok()?;
        Some(Self {
            path: path.to_owned(),
            original,
            changed: true,
        })
    }
}

impl Drop for CompactionGuard {
    fn drop(&mut self) {
        if self.changed {
            let _ = fs::write(&self.path, format!("{}\n", self.original));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod alloc_error_help {
        use assert2::check;

        use super::*;

        #[test]
        fn mlock_has_help() {
            let e = AllocError::Mlock(nix::Error::EPERM);
            check!(e.help().is_some());
            let msg = e.help().unwrap();
            check!(msg.contains("cap_ipc_lock"));
            check!(msg.contains("setcap"));
        }

        #[test]
        fn mmap_no_help() {
            let e = AllocError::Mmap(nix::Error::ENOMEM);
            check!(e.help().is_none());
        }

        #[test]
        fn zero_size_no_help() {
            let e = AllocError::ZeroSize;
            check!(e.help().is_none());
        }
    }

    mod compaction_guard {
        use std::os::unix::fs::PermissionsExt;

        use assert2::{assert, check};
        use tempfile::NamedTempFile;

        use super::*;

        fn write_tempfile(content: &str) -> NamedTempFile {
            let f = NamedTempFile::new().unwrap();
            fs::write(f.path(), content).unwrap();
            f
        }

        #[test]
        fn already_zero_does_not_write() {
            let f = write_tempfile("0\n");

            assert!(let Some(guard) = CompactionGuard::with_path(f.path()));
            check!(!guard.changed);
            check!(guard.original == "0");
            drop(guard);

            let content = fs::read_to_string(f.path()).unwrap();
            check!(content == "0\n");
        }

        #[test]
        fn nonzero_writes_zero_and_restores() {
            let f = write_tempfile("1\n");

            {
                assert!(let Some(guard) = CompactionGuard::with_path(f.path()));
                check!(guard.changed);
                check!(guard.original == "1");

                let content = fs::read_to_string(f.path()).unwrap();
                check!(content == "0\n");
            }

            let restored = fs::read_to_string(f.path()).unwrap();
            check!(restored == "1\n");
        }

        #[test]
        fn missing_path_returns_none() {
            let guard = CompactionGuard::with_path(Path::new("/tmp/ferrite_nonexistent_sysctl"));
            assert!(guard.is_none());
        }

        #[test]
        fn read_only_path_returns_none() {
            let f = write_tempfile("1\n");

            // Make read-only (owner r--, no write)
            let perms = std::fs::Permissions::from_mode(0o444);
            fs::set_permissions(f.path(), perms).unwrap();

            let guard = CompactionGuard::with_path(f.path());
            assert!(guard.is_none());
        }
    }
}
