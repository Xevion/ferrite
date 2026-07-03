use std::ffi::c_void;
use std::fs::OpenOptions;
use std::num::NonZeroUsize;
use std::ptr::NonNull;

use nix::sys::mman::{
    MapFlags, MmapAdvise, ProtFlags, madvise, mlock, mmap, mmap_anonymous, mprotect, munmap,
};
use rayon::prelude::*;
use snafu::{ResultExt, Snafu};

#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum AllocError {
    #[snafu(display("requested size must be non-zero"))]
    ZeroSize,
    #[snafu(display("mmap failed: {source}"))]
    Mmap { source: nix::Error },
    #[snafu(display("mlock failed (are you root or do you have CAP_IPC_LOCK?): {source}"))]
    Mlock { source: nix::Error },
    #[snafu(display("mprotect failed while activating chunk: {source}"))]
    Mprotect { source: nix::Error },
    #[snafu(display(
        "could not lock any memory below the headroom floor ({available} bytes available)"
    ))]
    Exhausted { available: u64 },
    #[snafu(display(
        "/dev/mem physical range must be page-aligned (start {phys_start:#x}, len {len:#x})"
    ))]
    DevMemAlignment { phys_start: u64, len: usize },
    #[snafu(display("could not open /dev/mem: {source}"))]
    DevMemOpen { source: std::io::Error },
    #[snafu(display("could not map physical range through /dev/mem: {source}"))]
    DevMemMap { source: nix::Error },
}

impl AllocError {
    /// Human-readable remediation hint for this error variant, if one exists.
    /// Callers may display this alongside the error message to guide the user.
    #[must_use]
    pub const fn help(&self) -> Option<&'static str> {
        match self {
            Self::Mlock { .. } => Some(
                "run as root, raise the mlock limit (ulimit -l unlimited), \
                or grant the capability: sudo setcap cap_ipc_lock+ep $(which ferrite)",
            ),
            Self::Exhausted { .. } => Some(
                "free memory (stop services, drop caches) or lower --headroom \
                to allow allocation closer to the limit",
            ),
            Self::DevMemMap { .. } => Some(
                "/dev/mem RAM access requires a kernel built with CONFIG_STRICT_DEVMEM=n \
                (Unraid and some appliance kernels); most distros block it",
            ),
            Self::DevMemOpen { .. } => Some("run as root to open /dev/mem"),
            Self::Mmap { .. }
            | Self::Mprotect { .. }
            | Self::ZeroSize
            | Self::DevMemAlignment { .. } => None,
        }
    }
}

/// Why the chunked allocation walk stopped.
#[derive(Debug)]
pub enum StopReason {
    /// The full requested size was activated and locked.
    Completed,
    /// `MemAvailable` dropped below the headroom floor before the next chunk.
    HeadroomFloor { available: u64 },
    /// Activating a chunk failed (mprotect, or mlock); the walk kept what it had.
    ChunkFailed(AllocError),
}

/// Result of a budgeted, chunked allocation: how much of the request was
/// actually activated and locked, and why the walk stopped.
#[derive(Debug)]
pub struct AllocOutcome {
    pub requested: usize,
    pub achieved: usize,
    pub stop: StopReason,
}

/// Chunk granularity for budgeted allocation.
///
/// Large enough that per-chunk syscall overhead vanishes, small enough that
/// the headroom floor check between chunks reacts before memory pressure
/// becomes an OOM kill.
pub const CHUNK_BYTES: usize = 512 * 1024 * 1024;

/// Walk `total` bytes in `chunk`-sized steps, activating each chunk in turn.
///
/// Before each chunk, `available()` is consulted (None = no reading, no cap):
/// if fewer than `headroom + chunk_len` bytes are available, the walk stops.
/// If `activate(offset, len)` fails, the walk stops and keeps prior chunks.
/// Returns the byte count successfully activated and the stop reason.
pub(crate) fn walk_chunks(
    total: usize,
    chunk: usize,
    headroom: u64,
    available: &mut dyn FnMut() -> Option<u64>,
    activate: &mut dyn FnMut(usize, usize) -> Result<(), AllocError>,
) -> (usize, StopReason) {
    let mut achieved = 0usize;
    while achieved < total {
        let chunk_len = chunk.min(total - achieved);
        if let Some(avail) = available()
            && avail < headroom.saturating_add(chunk_len as u64)
        {
            return (achieved, StopReason::HeadroomFloor { available: avail });
        }
        if let Err(e) = activate(achieved, chunk_len) {
            return (achieved, StopReason::ChunkFailed(e));
        }
        achieved += chunk_len;
    }
    (achieved, StopReason::Completed)
}

/// Activate one chunk of a `PROT_NONE` reservation based at `raw`: make it
/// writable, hint THP backing, fault every page in parallel, and lock it.
#[cfg_attr(coverage_nightly, coverage(off))]
pub(crate) fn activate_chunk(raw: usize, offset: usize, len: usize) -> Result<(), AllocError> {
    // SAFETY: `raw` is a valid non-null mapping base and `offset` is within
    // the reservation, so the sum cannot wrap to zero.
    let chunk = unsafe { NonNull::new_unchecked((raw + offset) as *mut c_void) };
    // SAFETY: [offset, offset+len) lies within the reservation.
    unsafe {
        mprotect(chunk, len, ProtFlags::PROT_READ | ProtFlags::PROT_WRITE)
            .context(MprotectSnafu)?;
    }
    // Hint 2 MiB THP backing before faulting. Ignored when THP is off.
    #[cfg(target_os = "linux")]
    // SAFETY: chunk range is a valid mapping owned by this reservation.
    unsafe {
        let _ = madvise(chunk, len, MmapAdvise::MADV_HUGEPAGE);
    }
    let page_count = len / 4096;
    (0..page_count).into_par_iter().for_each(|i| {
        // SAFETY: offset is within [0, len), each page touched exactly once.
        unsafe { ((raw + offset + i * 4096) as *mut u8).write_volatile(0u8) };
    });
    // SAFETY: chunk range is valid and faulted; mlock pins it in RAM.
    unsafe {
        mlock(chunk, len).context(MlockSnafu)?;
    }
    Ok(())
}

/// The full anonymous memory allocation that ferrite mmap's and mlock's.
/// Automatically unmaps on drop.
pub struct TestBuffer {
    ptr: NonNull<c_void>,
    len: usize,
}

// SAFETY: The memory region is exclusively owned and not shared across threads
// without synchronization. Raw pointer access is confined to this struct.
unsafe impl Send for TestBuffer {}

impl TestBuffer {
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
            .context(MmapSnafu)?
        };

        let len = size.get();

        // Hint the kernel to back this region with 2 MiB transparent huge pages.
        // Must be called before pages are faulted in so the kernel uses huge pages
        // at fault time rather than collapsing 4 KiB pages later via khugepaged.
        // Silently ignored if THP is disabled system-wide.
        #[cfg(target_os = "linux")]
        // SAFETY: `ptr` is valid for `len` bytes from the mmap above; madvise
        // does not require the pages to be faulted in yet.
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
            mlock(ptr, len).context(MlockSnafu)?;
        }

        Ok(Self { ptr, len })
    }

    /// Allocate up to `requested` bytes, activating and locking in
    /// [`CHUNK_BYTES`] steps so an over-sized request degrades to a smaller
    /// locked buffer instead of an OOM kill.
    ///
    /// The full request is reserved as inaccessible address space up front;
    /// each chunk is made writable, faulted in parallel, and locked. The walk
    /// stops when `MemAvailable` drops below `headroom` plus the next chunk,
    /// or when a chunk fails to activate. The unactivated tail is unmapped, so
    /// the resulting buffer is virtually contiguous with `len == achieved`.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError`] if the size is zero, the reservation fails, or
    /// no chunk at all could be locked.
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub fn new_budgeted(
        requested: usize,
        headroom: u64,
    ) -> Result<(Self, AllocOutcome), AllocError> {
        let size = NonZeroUsize::new(requested).ok_or(AllocError::ZeroSize)?;

        // Reserve address space only: PROT_NONE pages carry no commit charge,
        // so a 32 GiB reservation is free until chunks are activated.
        // SAFETY: anonymous private reservation, no existing mapping is replaced.
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

        let (achieved, stop) = walk_chunks(
            requested,
            CHUNK_BYTES,
            headroom,
            &mut || crate::physmem::sysmem::mem_available(),
            &mut |offset, len| activate_chunk(raw, offset, len),
        );

        if achieved == 0 {
            // SAFETY: unmapping the reservation we just created.
            unsafe {
                let _ = munmap(ptr, requested);
            }
            return Err(match stop {
                StopReason::ChunkFailed(e) => e,
                StopReason::HeadroomFloor { available } => AllocError::Exhausted { available },
                StopReason::Completed => unreachable!("zero-size requests are rejected above"),
            });
        }

        if achieved < requested {
            // SAFETY: `raw` is a valid non-null mapping base and
            // `achieved < requested`, so the sum cannot wrap to zero.
            let tail = unsafe { NonNull::new_unchecked((raw + achieved) as *mut c_void) };
            // SAFETY: [achieved, requested) is the untouched remainder of the
            // reservation; unmapping it leaves [0, achieved) intact.
            unsafe {
                let _ = munmap(tail, requested - achieved);
            }
        }

        Ok((
            Self { ptr, len: achieved },
            AllocOutcome {
                requested,
                achieved,
                stop,
            },
        ))
    }

    /// Map a physical address range directly through `/dev/mem`.
    ///
    /// `phys_start` and `len` must both be page-aligned. `writable` selects
    /// `PROT_READ|PROT_WRITE` (for destructive pattern testing) versus a
    /// read-only probe. The mapping is `MAP_SHARED` so writes reach physical
    /// memory. No `mlock` is needed — a `/dev/mem` mapping already points at
    /// fixed physical frames.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::DevMemAlignment`] if unaligned,
    /// [`AllocError::DevMemOpen`] if `/dev/mem` cannot be opened (not root),
    /// or [`AllocError::DevMemMap`] if the mapping fails (e.g.
    /// `CONFIG_STRICT_DEVMEM=y` blocks RAM access).
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub fn map_physical(phys_start: u64, len: usize, writable: bool) -> Result<Self, AllocError> {
        let size = NonZeroUsize::new(len).ok_or(AllocError::ZeroSize)?;
        if !phys_start.is_multiple_of(4096) || !len.is_multiple_of(4096) {
            return Err(AllocError::DevMemAlignment { phys_start, len });
        }
        let offset = i64::try_from(phys_start)
            .map_err(|_| AllocError::DevMemAlignment { phys_start, len })?;

        let file = OpenOptions::new()
            .read(true)
            .write(writable)
            .open("/dev/mem")
            .context(DevMemOpenSnafu)?;

        let prot = if writable {
            ProtFlags::PROT_READ | ProtFlags::PROT_WRITE
        } else {
            ProtFlags::PROT_READ
        };

        // SAFETY: /dev/mem is opened for the requested access; the range is
        // page-aligned; MAP_SHARED routes writes to physical memory. The kernel
        // validates the physical range and fails cleanly (EPERM) if disallowed.
        let ptr = unsafe {
            mmap(None, size, prot, MapFlags::MAP_SHARED, &file, offset).context(DevMemMapSnafu)?
        };
        Ok(Self { ptr, len })
    }

    /// Returns the buffer as a mutable slice of u64 words.
    /// The returned length is `self.len / 8` (trailing bytes are excluded).
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub const fn as_u64_slice_mut(&mut self) -> &mut [u64] {
        let word_count = self.len / size_of::<u64>();
        // SAFETY: The allocation is aligned to page boundaries (4096), which satisfies
        // u64 alignment (8). word_count * 8 <= self.len, so all accesses are in bounds.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr().cast::<u64>(), word_count) }
    }

    /// Returns the buffer as a slice of u64 words.
    #[must_use]
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub const fn as_u64_slice(&self) -> &[u64] {
        let word_count = self.len / size_of::<u64>();
        // SAFETY: Same alignment and bounds reasoning as as_u64_slice_mut.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr() as *const u64, word_count) }
    }

    /// The base virtual address of the allocation.
    #[must_use]
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub fn as_ptr(&self) -> usize {
        self.ptr.as_ptr() as usize
    }

    /// The size in bytes of the allocation.
    #[must_use]
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Always returns `false` -- the constructor rejects zero-size allocations.
    #[must_use]
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub const fn is_empty(&self) -> bool {
        false
    }
}

impl Drop for TestBuffer {
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
            let e = AllocError::Mlock {
                source: nix::Error::EPERM,
            };
            check!(e.help().is_some());
            let msg = e.help().unwrap();
            check!(msg.contains("cap_ipc_lock"));
            check!(msg.contains("setcap"));
        }

        #[test]
        fn mmap_no_help() {
            let e = AllocError::Mmap {
                source: nix::Error::ENOMEM,
            };
            check!(e.help().is_none());
        }

        #[test]
        fn zero_size_no_help() {
            let e = AllocError::ZeroSize;
            check!(e.help().is_none());
        }
    }

    mod walk_chunks {
        use assert2::{assert, check};

        use super::super::{AllocError, StopReason, walk_chunks};

        /// Record every activation call; fail those whose offset is in `fail_at`.
        fn recording_activate(
            calls: std::rc::Rc<std::cell::RefCell<Vec<(usize, usize)>>>,
            fail_at: Option<usize>,
        ) -> impl FnMut(usize, usize) -> Result<(), AllocError> {
            move |offset, len| {
                if fail_at == Some(offset) {
                    return Err(AllocError::Mlock {
                        source: nix::Error::ENOMEM,
                    });
                }
                calls.borrow_mut().push((offset, len));
                Ok(())
            }
        }

        #[test]
        fn full_request_completes_with_short_tail() {
            let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let mut activate = recording_activate(calls.clone(), None);
            let (achieved, stop) = walk_chunks(10, 4, 0, &mut || None, &mut activate);
            check!(achieved == 10);
            assert!(let StopReason::Completed = stop);
            check!(*calls.borrow() == vec![(0, 4), (4, 4), (8, 2)]);
        }

        #[test]
        fn headroom_floor_stops_walk() {
            let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let mut activate = recording_activate(calls.clone(), None);
            // First check: plenty. Second check: 90 < headroom(100) + chunk(4).
            let mut readings = vec![200u64, 90].into_iter();
            let (achieved, stop) = walk_chunks(8, 4, 100, &mut || readings.next(), &mut activate);
            check!(achieved == 4);
            assert!(let StopReason::HeadroomFloor { available: 90 } = stop);
            check!(*calls.borrow() == vec![(0, 4)]);
        }

        #[test]
        fn chunk_failure_keeps_prior_chunks() {
            let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let mut activate = recording_activate(calls.clone(), Some(4));
            let (achieved, stop) = walk_chunks(12, 4, 0, &mut || None, &mut activate);
            check!(achieved == 4);
            assert!(let StopReason::ChunkFailed(AllocError::Mlock { .. }) = stop);
            check!(*calls.borrow() == vec![(0, 4)]);
        }

        #[test]
        fn unreadable_meminfo_means_no_cap() {
            let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let mut activate = recording_activate(calls, None);
            let (achieved, stop) = walk_chunks(8, 4, u64::MAX, &mut || None, &mut activate);
            check!(achieved == 8);
            assert!(let StopReason::Completed = stop);
        }

        #[test]
        fn floor_can_stop_before_first_chunk() {
            let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let mut activate = recording_activate(calls.clone(), None);
            let (achieved, stop) = walk_chunks(8, 4, 1000, &mut || Some(500), &mut activate);
            check!(achieved == 0);
            assert!(let StopReason::HeadroomFloor { available: 500 } = stop);
            check!(calls.borrow().is_empty());
        }

        #[test]
        fn request_smaller_than_chunk_is_single_call() {
            let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let mut activate = recording_activate(calls.clone(), None);
            let (achieved, stop) = walk_chunks(3, 4, 0, &mut || None, &mut activate);
            check!(achieved == 3);
            assert!(let StopReason::Completed = stop);
            check!(*calls.borrow() == vec![(0, 3)]);
        }
    }

    mod budgeted {
        use assert2::{assert, check};

        use super::super::{AllocError, StopReason, TestBuffer};

        #[test]
        fn small_budgeted_allocation_completes() {
            // 4 MiB fits default RLIMIT_MEMLOCK on modern systems. If this
            // environment forbids mlock outright, that's an env limitation,
            // not a code failure -- the walk logic is covered by walk_chunks.
            match TestBuffer::new_budgeted(4 * 1024 * 1024, 0) {
                Ok((buf, outcome)) => {
                    check!(buf.len() == 4 * 1024 * 1024);
                    check!(outcome.achieved == outcome.requested);
                    assert!(let StopReason::Completed = outcome.stop);
                }
                Err(AllocError::Mlock { .. }) => {
                    eprintln!("skipping: mlock not permitted in this environment");
                }
                Err(e) => panic!("unexpected alloc error: {e}"),
            }
        }

        #[test]
        fn zero_size_is_rejected() {
            let Err(e) = TestBuffer::new_budgeted(0, 0) else {
                panic!("expected ZeroSize error");
            };
            assert!(let AllocError::ZeroSize = e);
        }

        #[test]
        fn impossible_headroom_is_exhausted() {
            // A u64::MAX headroom floor can never be satisfied, so no chunk
            // activates and the constructor reports exhaustion with help text.
            let Err(e) = TestBuffer::new_budgeted(4 * 1024 * 1024, u64::MAX) else {
                panic!("expected Exhausted error");
            };
            check!(e.help().is_some());
            assert!(let AllocError::Exhausted { .. } = e);
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
