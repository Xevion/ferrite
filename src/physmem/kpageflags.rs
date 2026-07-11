//! Unified `/proc/kpageflags` bit table, flag accessor, and readers.
//!
//! `/proc/kpageflags` is a flat array of 64-bit flag words indexed by PFN
//! (8 bytes per frame, root-readable). This module owns the single canonical
//! [`KPageFlags`] bit table used by page-map statistics ([`super::phys::MapStats`]),
//! and both readers: `read_one` for a single frame and `read_batch` for the
//! large sequential scans that gap classification ([`super::gap`]) performs.

use std::fs::File;

use bitflags::bitflags;
use snafu::ResultExt;

use super::pfn::{Pfn, PfnRange};
use super::phys::{PhysError, ReadKpageflagsSnafu, pread_exact};

bitflags! {
    /// Public /proc/kpageflags bits (Documentation/admin-guide/mm/pagemap.rst).
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct KPageFlags: u64 {
        const LRU = 1 << 5;
        const SLAB = 1 << 7;
        const BUDDY = 1 << 10;
        const ANON = 1 << 12;
        const SWAPBACKED = 1 << 14;
        const HUGE = 1 << 17;
        const UNEVICTABLE = 1 << 18;
        const HWPOISON = 1 << 19;
        const NOPAGE = 1 << 20;
        const THP = 1 << 22;
        const OFFLINE = 1 << 23;
        const PGTABLE = 1 << 26;
    }
}

impl KPageFlags {
    #[must_use]
    pub const fn is_huge(self) -> bool {
        self.contains(Self::HUGE)
    }

    #[must_use]
    pub const fn is_thp(self) -> bool {
        self.contains(Self::THP)
    }

    #[must_use]
    pub const fn is_unevictable(self) -> bool {
        self.contains(Self::UNEVICTABLE)
    }

    #[must_use]
    pub const fn is_hwpoison(self) -> bool {
        self.contains(Self::HWPOISON)
    }
}

/// Frames per batched `/proc/kpageflags` read (512 KiB of flag data).
pub const READ_BATCH_FRAMES: u64 = 65536;

/// Read the flag word for a single frame from an open `/proc/kpageflags`.
///
/// # Errors
///
/// Returns [`PhysError::ReadKpageflags`] if the flag word cannot be read.
pub(crate) fn read_one(fd: &File, pfn: Pfn) -> Result<KPageFlags, PhysError> {
    let mut buf = [0u8; 8];
    pread_exact(fd, &mut buf, pfn.kpageflags_offset() as i64).context(ReadKpageflagsSnafu)?;
    Ok(KPageFlags::from_bits_retain(u64::from_le_bytes(buf)))
}

/// Read the flag words for a contiguous `range` of frames into `out` (one word
/// per frame). `scratch` is reused byte storage at least `range.count * 8`
/// bytes long. Returns `false` on any read failure, leaving `out` untouched.
pub(crate) fn read_batch(fd: &File, range: PfnRange, scratch: &mut [u8], out: &mut [u64]) -> bool {
    let Ok(len) = usize::try_from(range.count * 8) else {
        return false;
    };
    let Ok(offset) = i64::try_from(range.start.kpageflags_offset()) else {
        return false;
    };
    if pread_exact(fd, &mut scratch[..len], offset).is_err() {
        return false;
    }
    for (slot, chunk) in out.iter_mut().zip(scratch[..len].chunks_exact(8)) {
        // chunks_exact(8) guarantees the conversion; 0 is unreachable.
        *slot = chunk.try_into().map_or(0, u64::from_le_bytes);
    }
    true
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use assert2::{assert, check};
    use tempfile::NamedTempFile;

    use super::*;

    fn write_flag_file(words: &[u64]) -> File {
        let mut f = NamedTempFile::new().unwrap();
        for &w in words {
            f.write_all(&w.to_le_bytes()).unwrap();
        }
        f.flush().unwrap();
        f.reopen().unwrap()
    }

    #[test]
    fn page_flags_methods() {
        let flags = KPageFlags::HUGE | KPageFlags::THP;
        check!(flags.is_huge());
        check!(flags.is_thp());
        check!(!flags.is_unevictable());
        check!(!flags.is_hwpoison());

        let flags2 = KPageFlags::UNEVICTABLE | KPageFlags::HWPOISON;
        check!(flags2.is_unevictable());
        check!(flags2.is_hwpoison());
        check!(!flags2.is_huge());
    }

    #[test]
    fn read_one_returns_the_frames_word() {
        let fd = write_flag_file(&[
            0,
            KPageFlags::BUDDY.bits(),
            (KPageFlags::ANON | KPageFlags::LRU).bits(),
        ]);
        assert!(let Ok(flags) = read_one(&fd, Pfn::new(1)));
        check!(flags == KPageFlags::BUDDY);
        assert!(let Ok(flags2) = read_one(&fd, Pfn::new(2)));
        check!(flags2 == KPageFlags::ANON | KPageFlags::LRU);
    }

    #[test]
    fn read_one_past_end_errors() {
        let fd = write_flag_file(&[KPageFlags::BUDDY.bits()]);
        check!(read_one(&fd, Pfn::new(9)).is_err());
    }

    #[test]
    fn read_batch_fills_out_in_order() {
        let words = [
            KPageFlags::BUDDY.bits(),
            KPageFlags::LRU.bits(),
            KPageFlags::ANON.bits(),
            KPageFlags::SLAB.bits(),
        ];
        let fd = write_flag_file(&words);
        let mut scratch = vec![0u8; 4 * 8];
        let mut out = [0u64; 3];
        let ok = read_batch(
            &fd,
            PfnRange {
                start: Pfn::new(1),
                count: 3,
            },
            &mut scratch,
            &mut out,
        );
        check!(ok);
        check!(
            out == [
                KPageFlags::LRU.bits(),
                KPageFlags::ANON.bits(),
                KPageFlags::SLAB.bits()
            ]
        );
    }

    #[test]
    fn read_batch_past_end_returns_false() {
        let fd = write_flag_file(&[KPageFlags::BUDDY.bits()]);
        let mut scratch = vec![0u8; 8 * 8];
        let mut out = [0u64; 8];
        let ok = read_batch(
            &fd,
            PfnRange {
                start: Pfn::new(0),
                count: 8,
            },
            &mut scratch,
            &mut out,
        );
        check!(!ok);
    }
}
