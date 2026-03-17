use std::ffi::c_void;
use std::num::NonZeroUsize;
use std::ptr::NonNull;

use nix::sys::mman::{MapFlags, ProtFlags, mlock, mmap_anonymous, munmap};
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
    /// Pages are faulted in via volatile writes before returning.
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

        let raw = ptr.as_ptr() as *mut u8;
        let len = size.get();

        // SAFETY: ptr is valid for len bytes. mlock pins pages in physical RAM.
        unsafe {
            mlock(ptr, len).map_err(AllocError::Mlock)?;
        }

        // Force the kernel to back every page with physical RAM by touching each one.
        let mut offset = 0;
        while offset < len {
            // SAFETY: offset < len, so raw.add(offset) is within the allocation.
            unsafe { raw.add(offset).write_volatile(0u8) };
            offset += 4096;
        }

        Ok(Self { ptr, len })
    }

    /// Returns the buffer as a mutable slice of u64 words.
    /// The returned length is `self.len / 8` (trailing bytes are excluded).
    pub fn as_u64_slice_mut(&mut self) -> &mut [u64] {
        let word_count = self.len / size_of::<u64>();
        // SAFETY: The allocation is aligned to page boundaries (4096), which satisfies
        // u64 alignment (8). word_count * 8 <= self.len, so all accesses are in bounds.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr() as *mut u64, word_count) }
    }

    /// Returns the buffer as a slice of u64 words.
    pub fn as_u64_slice(&self) -> &[u64] {
        let word_count = self.len / size_of::<u64>();
        // SAFETY: Same alignment and bounds reasoning as as_u64_slice_mut.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr() as *const u64, word_count) }
    }

    /// The size in bytes of the locked region.
    /// A `LockedRegion` is never empty (the constructor rejects zero size).
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Raw pointer to the start of the region.
    pub fn as_ptr(&self) -> *const u8 {
        self.ptr.as_ptr() as *const u8
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
