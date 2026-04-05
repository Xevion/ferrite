use std::fmt;
use std::fs::File;
use std::io;
use std::os::unix::io::AsRawFd;

use nix::libc;
use serde::Serialize;
use thiserror::Error;

/// A physical address. Newtype to prevent mixing with virtual addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(into = "String")]
pub struct PhysAddr(pub u64);

impl PhysAddr {
    /// The page frame number (PFN) -- physical address >> 12.
    #[must_use]
    pub fn pfn(self) -> u64 {
        self.0 >> 12
    }

    /// The page offset -- lower 12 bits.
    #[must_use]
    pub fn page_offset(self) -> u64 {
        self.0 & 0xFFF
    }
}

impl From<PhysAddr> for String {
    fn from(addr: PhysAddr) -> Self {
        format!("0x{:x}", addr.0)
    }
}

impl fmt::Display for PhysAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:x}", self.0)
    }
}

impl fmt::LowerHex for PhysAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::LowerHex::fmt(&self.0, f)
    }
}

impl fmt::UpperHex for PhysAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::UpperHex::fmt(&self.0, f)
    }
}

#[derive(Debug, Error)]
pub enum PhysError {
    #[error("failed to open /proc/self/pagemap: {0}")]
    OpenPagemap(#[source] io::Error),
    #[error("failed to read pagemap entries: {0}")]
    ReadPagemap(#[source] io::Error),
    #[error("page not present at virtual address 0x{0:x}")]
    PageNotPresent(usize),
    #[error("PFN not available (requires CAP_SYS_ADMIN) at virtual address 0x{0:x}")]
    PfnUnavailable(usize),
    #[error("failed to open /proc/kpageflags: {0}")]
    OpenKpageflags(#[source] io::Error),
    #[error("failed to read kpageflags: {0}")]
    ReadKpageflags(#[source] io::Error),
}

// Pagemap entry bit layout (kernel 4.2+, x86_64)
const PM_PFN_MASK: u64 = (1 << 55) - 1; // bits 0-54
const PM_PRESENT: u64 = 1 << 63; // bit 63
const PM_SWAP: u64 = 1 << 62; // bit 62

const PAGE_SIZE: usize = 4096;

/// Page flags from /proc/kpageflags, indexed by PFN.
#[derive(Debug, Clone, Copy, Default)]
pub struct PageFlags {
    pub raw: u64,
}

impl PageFlags {
    const KPF_HUGE: u64 = 1 << 17;
    const KPF_UNEVICTABLE: u64 = 1 << 18;
    const KPF_HWPOISON: u64 = 1 << 19;
    const KPF_THP: u64 = 1 << 22;

    #[must_use]
    pub fn is_huge(self) -> bool {
        self.raw & Self::KPF_HUGE != 0
    }

    #[must_use]
    pub fn is_thp(self) -> bool {
        self.raw & Self::KPF_THP != 0
    }

    #[must_use]
    pub fn is_unevictable(self) -> bool {
        self.raw & Self::KPF_UNEVICTABLE != 0
    }

    #[must_use]
    pub fn is_hwpoison(self) -> bool {
        self.raw & Self::KPF_HWPOISON != 0
    }
}

/// Statistics from building the page map.
#[derive(Debug)]
pub struct MapStats {
    pub total_pages: usize,
    pub huge_pages: usize,
    pub thp_pages: usize,
    pub hwpoison_pages: usize,
    pub unevictable_pages: usize,
}

/// Resolves virtual addresses within a locked region to physical addresses.
pub trait PhysResolver {
    /// Build the internal mapping for a contiguous virtual region.
    /// Called once after mlock + initial write pass.
    ///
    /// # Errors
    ///
    /// Returns [`PhysError`] if pagemap cannot be read or pages are not present.
    fn build_map(&mut self, base: usize, len: usize) -> Result<MapStats, PhysError>;

    /// Resolve a single virtual address to a physical address.
    ///
    /// # Errors
    ///
    /// Returns [`PhysError`] if the pagemap entry cannot be read or the page
    /// is not present.
    fn resolve(&self, vaddr: usize) -> Result<PhysAddr, PhysError>;

    /// Query kpageflags for a given PFN.
    ///
    /// # Errors
    ///
    /// Returns [`PhysError`] if `/proc/kpageflags` cannot be read.
    fn page_flags(&self, pfn: u64) -> Result<PageFlags, PhysError>;

    /// Verify that PFN mappings haven't changed since `build_map`.
    /// Returns the number of pages whose PFN changed (0 = stable).
    ///
    /// # Errors
    ///
    /// Returns [`PhysError`] if the pagemap cannot be re-read for comparison.
    fn verify_stability(&self, base: usize, len: usize) -> Result<usize, PhysError>;
}

/// Resolves virtual -> physical addresses via /proc/self/pagemap.
pub struct PagemapResolver {
    pagemap_fd: File,
    kpageflags_fd: Option<File>,
    /// Cached PFNs indexed by page offset from the region base.
    pfns: Vec<u64>,
    /// Base virtual address of the mapped region.
    region_base: usize,
}

impl PagemapResolver {
    /// Open `/proc/self/pagemap` (and optionally `/proc/kpageflags`) for
    /// virtual-to-physical address resolution.
    ///
    /// # Errors
    ///
    /// Returns [`PhysError::OpenPagemap`] if `/proc/self/pagemap` cannot be opened
    /// (typically requires root or `CAP_SYS_ADMIN`).
    pub fn new() -> Result<Self, PhysError> {
        let pagemap_fd = File::open("/proc/self/pagemap").map_err(PhysError::OpenPagemap)?;
        let kpageflags_fd = File::open("/proc/kpageflags").ok();
        Ok(Self {
            pagemap_fd,
            kpageflags_fd,
            pfns: Vec::new(),
            region_base: 0,
        })
    }
}

/// Parse a single 64-bit pagemap entry, returning the PFN if the page is present.
fn parse_pagemap_entry(entry: u64) -> Option<u64> {
    if entry & PM_PRESENT == 0 {
        return None;
    }
    if entry & PM_SWAP != 0 {
        return None;
    }
    let pfn = entry & PM_PFN_MASK;
    // PFN of 0 with present bit set means CAP_SYS_ADMIN is missing (kernel 4.2+)
    if pfn == 0 {
        return None;
    }
    Some(pfn)
}

/// Read exactly `buf.len()` bytes from `fd` at `offset` using pread.
/// Handles short reads by retrying.
///
/// The caller must ensure `offset + buf.len()` fits in `i64`. In practice
/// this holds: the largest pagemap offset is ~2^50 (128 TiB virtual address
/// space x 8 bytes per entry), well within i64 range.
fn pread_exact(fd: &File, buf: &mut [u8], offset: i64) -> io::Result<()> {
    let raw_fd = fd.as_raw_fd();
    let mut total = 0usize;
    while total < buf.len() {
        let adjusted_offset = offset
            .checked_add(total as i64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "pread offset overflow"))?;
        // SAFETY: fd is a valid file descriptor, buf is a valid mutable slice,
        // and the offset is within the file's addressable range.
        let n = unsafe {
            libc::pread(
                raw_fd,
                buf[total..].as_mut_ptr().cast(),
                buf.len() - total,
                adjusted_offset,
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "pread returned 0 bytes",
            ));
        }
        total += n as usize;
    }
    Ok(())
}

impl PhysResolver for PagemapResolver {
    fn build_map(&mut self, base: usize, len: usize) -> Result<MapStats, PhysError> {
        let page_count = len / PAGE_SIZE;
        let start_vpn = base / PAGE_SIZE;
        let file_offset = (start_vpn * 8) as i64;

        // Batch read: single pread for the entire region's pagemap entries
        let mut buf = vec![0u8; page_count * 8];
        pread_exact(&self.pagemap_fd, &mut buf, file_offset).map_err(PhysError::ReadPagemap)?;

        // Parse entries and extract PFNs
        let mut pfns = Vec::with_capacity(page_count);
        for i in 0..page_count {
            let off = i * 8;
            let entry = u64::from_le_bytes(buf[off..off + 8].try_into().unwrap());
            pfns.push(parse_pagemap_entry(entry).unwrap_or(0));
        }

        self.pfns = pfns;
        self.region_base = base;

        // Compute MapStats by reading kpageflags if available
        let mut stats = MapStats {
            total_pages: page_count,
            huge_pages: 0,
            thp_pages: 0,
            hwpoison_pages: 0,
            unevictable_pages: 0,
        };

        if self.kpageflags_fd.is_some() {
            for &pfn in &self.pfns {
                if pfn == 0 {
                    continue;
                }
                if let Ok(flags) = self.page_flags(pfn) {
                    if flags.is_huge() {
                        stats.huge_pages += 1;
                    }
                    if flags.is_thp() {
                        stats.thp_pages += 1;
                    }
                    if flags.is_hwpoison() {
                        stats.hwpoison_pages += 1;
                    }
                    if flags.is_unevictable() {
                        stats.unevictable_pages += 1;
                    }
                }
            }
        }

        Ok(stats)
    }

    fn resolve(&self, vaddr: usize) -> Result<PhysAddr, PhysError> {
        if vaddr < self.region_base {
            return Err(PhysError::PageNotPresent(vaddr));
        }
        let page_idx = (vaddr - self.region_base) / PAGE_SIZE;
        let pfn = *self
            .pfns
            .get(page_idx)
            .ok_or(PhysError::PageNotPresent(vaddr))?;
        if pfn == 0 {
            return Err(PhysError::PfnUnavailable(vaddr));
        }
        let offset = (vaddr as u64) & 0xFFF;
        Ok(PhysAddr((pfn << 12) | offset))
    }

    fn page_flags(&self, pfn: u64) -> Result<PageFlags, PhysError> {
        let fd = self.kpageflags_fd.as_ref().ok_or_else(|| {
            PhysError::OpenKpageflags(io::Error::new(
                io::ErrorKind::NotFound,
                "/proc/kpageflags not available",
            ))
        })?;

        let mut buf = [0u8; 8];
        pread_exact(fd, &mut buf, (pfn * 8) as i64).map_err(PhysError::ReadKpageflags)?;

        Ok(PageFlags {
            raw: u64::from_le_bytes(buf),
        })
    }

    fn verify_stability(&self, base: usize, len: usize) -> Result<usize, PhysError> {
        let page_count = len / PAGE_SIZE;
        let start_vpn = base / PAGE_SIZE;
        let file_offset = (start_vpn * 8) as i64;

        let mut buf = vec![0u8; page_count * 8];
        pread_exact(&self.pagemap_fd, &mut buf, file_offset).map_err(PhysError::ReadPagemap)?;

        let mut changed = 0usize;
        for i in 0..page_count {
            let off = i * 8;
            let entry = u64::from_le_bytes(buf[off..off + 8].try_into().unwrap());
            let new_pfn = parse_pagemap_entry(entry).unwrap_or(0);
            if new_pfn != self.pfns[i] {
                changed += 1;
            }
        }

        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phys_addr_pfn_and_offset() {
        let addr = PhysAddr(0x1234_5678);
        assert_eq!(addr.pfn(), 0x0001_2345);
        assert_eq!(addr.page_offset(), 0x678);
    }

    #[test]
    fn phys_addr_display() {
        let addr = PhysAddr(0xdead_beef);
        assert_eq!(format!("{addr}"), "0xdeadbeef");
        assert_eq!(format!("{addr:#x}"), "0xdeadbeef");
    }

    #[test]
    fn parse_present_page() {
        let entry: u64 = (1u64 << 63) | 0x12345;
        assert_eq!(parse_pagemap_entry(entry), Some(0x12345));
    }

    #[test]
    fn parse_not_present() {
        let entry: u64 = 0x12345;
        assert_eq!(parse_pagemap_entry(entry), None);
    }

    #[test]
    fn parse_swapped_page() {
        let entry: u64 = (1u64 << 63) | (1u64 << 62) | 0x12345;
        assert_eq!(parse_pagemap_entry(entry), None);
    }

    #[test]
    fn parse_zero_pfn_means_no_cap() {
        let entry: u64 = 1u64 << 63;
        assert_eq!(parse_pagemap_entry(entry), None);
    }

    #[test]
    fn parse_all_flags() {
        let entry: u64 = (1u64 << 63) | (1u64 << 56) | (1u64 << 55) | 0xABCDE;
        assert_eq!(parse_pagemap_entry(entry), Some(0xABCDE));
    }

    #[test]
    fn phys_addr_into_string() {
        let addr = PhysAddr(0x1234_5678);
        let s: String = addr.into();
        assert_eq!(s, "0x12345678");
    }

    #[test]
    fn phys_addr_upper_hex() {
        let addr = PhysAddr(0xdead_beef);
        assert_eq!(format!("{addr:X}"), "DEADBEEF");
        assert_eq!(format!("{addr:#X}"), "0xDEADBEEF");
    }

    #[test]
    fn phys_error_display() {
        let e = PhysError::PageNotPresent(0x1000);
        assert!(e.to_string().contains("0x1000"));

        let e = PhysError::PfnUnavailable(0x2000);
        assert!(e.to_string().contains("CAP_SYS_ADMIN"));

        let e = PhysError::OpenPagemap(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "denied",
        ));
        assert!(e.to_string().contains("pagemap"));

        let e =
            PhysError::OpenKpageflags(std::io::Error::new(std::io::ErrorKind::NotFound, "missing"));
        assert!(e.to_string().contains("kpageflags"));
    }

    #[test]
    fn page_flags_methods() {
        let flags = PageFlags {
            raw: (1 << 17) | (1 << 22),
        };
        assert!(flags.is_huge());
        assert!(flags.is_thp());
        assert!(!flags.is_unevictable());
        assert!(!flags.is_hwpoison());

        let flags2 = PageFlags {
            raw: (1 << 18) | (1 << 19),
        };
        assert!(flags2.is_unevictable());
        assert!(flags2.is_hwpoison());
        assert!(!flags2.is_huge());
    }

    mod pread_exact_tests {
        use std::io::Write;

        use assert2::{assert, check};
        use tempfile::NamedTempFile;

        use super::*;

        #[test]
        fn reads_exact_bytes_at_offset() {
            let mut f = NamedTempFile::new().unwrap();
            let data: Vec<u8> = (0..64).collect();
            f.write_all(&data).unwrap();
            f.flush().unwrap();

            let file = f.reopen().unwrap();
            let mut buf = [0u8; 8];
            pread_exact(&file, &mut buf, 16).unwrap();
            check!(buf == [16, 17, 18, 19, 20, 21, 22, 23]);
        }

        #[test]
        fn reads_from_start() {
            let mut f = NamedTempFile::new().unwrap();
            f.write_all(b"hello world").unwrap();
            f.flush().unwrap();

            let file = f.reopen().unwrap();
            let mut buf = [0u8; 5];
            pread_exact(&file, &mut buf, 0).unwrap();
            check!(buf == *b"hello");
        }

        #[test]
        fn eof_returns_error() {
            let mut f = NamedTempFile::new().unwrap();
            f.write_all(b"short").unwrap();
            f.flush().unwrap();

            let file = f.reopen().unwrap();
            let mut buf = [0u8; 32];
            let result = pread_exact(&file, &mut buf, 0);
            assert!(result.is_err());
        }

        #[test]
        fn offset_past_end_returns_error() {
            let mut f = NamedTempFile::new().unwrap();
            f.write_all(b"data").unwrap();
            f.flush().unwrap();

            let file = f.reopen().unwrap();
            let mut buf = [0u8; 1];
            let result = pread_exact(&file, &mut buf, 1000);
            assert!(result.is_err());
        }

        #[test]
        fn empty_read_succeeds() {
            let mut f = NamedTempFile::new().unwrap();
            f.write_all(b"data").unwrap();
            f.flush().unwrap();

            let file = f.reopen().unwrap();
            let mut buf = [0u8; 0];
            pread_exact(&file, &mut buf, 0).unwrap();
        }
    }

    /// A fake [`PhysResolver`] for testing code that consumes the trait
    /// without needing `/proc/self/pagemap`.
    ///
    /// `resolve` returns `PhysAddr(vaddr as u64 + phys_offset)`.
    pub(crate) struct FakeResolver {
        pub base: usize,
        pub len: usize,
        pub phys_offset: u64,
    }

    impl FakeResolver {
        pub fn new(base: usize, len: usize) -> Self {
            Self {
                base,
                len,
                phys_offset: 0x1_0000_0000,
            }
        }
    }

    impl PhysResolver for FakeResolver {
        fn build_map(&mut self, base: usize, len: usize) -> Result<MapStats, PhysError> {
            self.base = base;
            self.len = len;
            Ok(MapStats {
                total_pages: len / PAGE_SIZE,
                huge_pages: 0,
                thp_pages: 0,
                hwpoison_pages: 0,
                unevictable_pages: 0,
            })
        }

        fn resolve(&self, vaddr: usize) -> Result<PhysAddr, PhysError> {
            if vaddr < self.base || vaddr >= self.base + self.len {
                return Err(PhysError::PageNotPresent(vaddr));
            }
            Ok(PhysAddr(vaddr as u64 + self.phys_offset))
        }

        fn page_flags(&self, _pfn: u64) -> Result<PageFlags, PhysError> {
            Ok(PageFlags::default())
        }

        fn verify_stability(&self, _base: usize, _len: usize) -> Result<usize, PhysError> {
            Ok(0)
        }
    }

    mod fake_resolver_tests {
        use assert2::{assert, check};

        use super::*;

        #[test]
        fn resolve_within_range() {
            let resolver = FakeResolver::new(0x1000, 0x2000);
            let addr = resolver.resolve(0x1500).unwrap();
            check!(addr.0 == 0x1500u64 + 0x1_0000_0000);
        }

        #[test]
        fn resolve_out_of_range() {
            let resolver = FakeResolver::new(0x1000, 0x2000);
            assert!(resolver.resolve(0x5000).is_err());
        }

        #[test]
        fn build_map_returns_stats() {
            let mut resolver = FakeResolver::new(0, 0);
            let stats = resolver.build_map(0x1000, 8192).unwrap();
            check!(stats.total_pages == 2);
            check!(resolver.base == 0x1000);
            check!(resolver.len == 8192);
        }
    }
}
