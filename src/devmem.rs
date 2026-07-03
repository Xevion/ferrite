//! `/dev/mem` targeted physical testing backend.
//!
//! The normal path tests whatever physical pages the allocator hands out.
//! This backend maps a *chosen* physical range through `/dev/mem`, so you can
//! test a specific address — e.g. the neighborhood of a known-bad cell — or
//! the `memmap=`-reserved regions the kernel has carved out of RAM.
//!
//! # Safety model
//!
//! Writing test patterns to physical memory the kernel is using corrupts it
//! instantly. Every target is classified against `/proc/iomem` and the
//! `memmap=` reservations on `/proc/cmdline`:
//!
//! - [`Safety::Reserved`] — inside a `memmap=`-reserved region. The kernel
//!   does not touch it, so writes are safe. Allowed by default.
//! - [`Safety::SystemRam`] — live System RAM. Writing here can crash the
//!   machine; allowed only with `--devmem-unsafe`.
//! - [`Safety::FirmwareOrMmio`] — neither reserved-RAM nor System RAM (ACPI
//!   tables, PCI MMIO, firmware). Writing can brick hardware; **never**
//!   allowed, even with `--devmem-unsafe`.
//!
//! `/dev/mem` access itself requires `CONFIG_STRICT_DEVMEM=n` (Unraid and some
//! appliance kernels); on a locked-down kernel the mmap fails cleanly and the
//! backend reports itself unavailable.

use thiserror::Error;

use crate::phys::{MapStats, PageFlags, PhysAddr, PhysError, PhysResolver};

/// What the user asked `--devmem` to test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevMemTarget {
    /// An explicit inclusive physical byte range `[start, end]`.
    Range { start: u64, end: u64 },
    /// Every `memmap=`-reserved region on the kernel cmdline.
    Reserved,
}

/// A concrete physical range to map, with its resolved write-safety verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mapping {
    pub phys_start: u64,
    pub len: usize,
    pub safety: Safety,
}

/// Failure to turn a [`DevMemTarget`] into concrete [`Mapping`]s.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum DevMemError {
    #[error("--devmem reserved: no memmap= reservations found on the kernel cmdline")]
    NoReservedRegions,
    #[error("--devmem range must be page-aligned (start {start:#x}, len {len:#x})")]
    Unaligned { start: u64, len: usize },
}

/// Result of a read-only reachability probe over a physical range.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ProbeStats {
    pub words_read: usize,
    pub nonzero_words: usize,
    pub xor_checksum: u64,
}

impl ProbeStats {
    /// Combine two probe results, e.g. from consecutive `pread` chunks.
    #[must_use]
    pub const fn merge(self, other: Self) -> Self {
        Self {
            words_read: self.words_read + other.words_read,
            nonzero_words: self.nonzero_words + other.nonzero_words,
            xor_checksum: self.xor_checksum ^ other.xor_checksum,
        }
    }
}

/// The write-safety classification of a physical range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Safety {
    /// Inside a `memmap=`-reserved region: safe to write.
    Reserved,
    /// Live System RAM: writable only under `--devmem-unsafe`.
    SystemRam,
    /// ACPI / PCI MMIO / firmware: never writable.
    FirmwareOrMmio,
}

/// Parse a `--devmem` argument: either `reserved` or an inclusive hex range
/// `START-END` (e.g. `0x39400000-0x395fffff`).
///
/// # Errors
///
/// Returns a human-readable message if the value is neither `reserved` nor a
/// well-formed `START-END` pair with `END >= START`.
pub fn parse_target(s: &str) -> Result<DevMemTarget, String> {
    if s.eq_ignore_ascii_case("reserved") {
        return Ok(DevMemTarget::Reserved);
    }
    let (start, end) = s.split_once('-').ok_or_else(|| {
        format!("invalid --devmem value: {s} (expected \"reserved\" or START-END, e.g. 0x39400000-0x395fffff)")
    })?;
    let start = parse_addr(start.trim())?;
    let end = parse_addr(end.trim())?;
    if end < start {
        return Err(format!(
            "invalid --devmem range: end {end:#x} is below start {start:#x}"
        ));
    }
    Ok(DevMemTarget::Range { start, end })
}

/// Parse a physical address as hexadecimal, with or without a `0x` prefix.
/// Bare hex matches the format `/proc/iomem` prints, so a range can be pasted
/// straight from it.
fn parse_addr(s: &str) -> Result<u64, String> {
    let hex = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    u64::from_str_radix(hex, 16).map_err(|_| format!("invalid physical address: {s}"))
}

/// Extract the inclusive `[start, end]` byte ranges of every `memmap=`
/// *reservation* (`memmap=SIZE$ADDR`) on a kernel command line. Only the `$`
/// (reserve) form is returned; `@`/`#`/`!` forms are ignored.
#[must_use]
pub fn parse_memmap_reserved(cmdline: &str) -> Vec<(u64, u64)> {
    cmdline
        .split_whitespace()
        .filter_map(|tok| tok.strip_prefix("memmap="))
        .filter_map(parse_memmap_reservation)
        .collect()
}

/// Parse the body of one `memmap=` token (`SIZE$ADDR`) into an inclusive
/// `[start, end]` range. Returns `None` unless the separator is `$` (reserve).
fn parse_memmap_reservation(body: &str) -> Option<(u64, u64)> {
    let (size_str, addr_str) = body.split_once('$')?;
    let size = parse_memmap_size(size_str)?;
    let addr = parse_addr(addr_str.trim()).ok()?;
    if size == 0 {
        return None;
    }
    Some((addr, addr + size - 1))
}

/// Parse a `memmap=` size: a decimal count with an optional binary `K`/`M`/`G`
/// suffix (as the kernel documents for the cmdline).
fn parse_memmap_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let (digits, mult) = match s.as_bytes().last()? {
        b'K' | b'k' => (&s[..s.len() - 1], 1024),
        b'M' | b'm' => (&s[..s.len() - 1], 1024 * 1024),
        b'G' | b'g' => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1),
    };
    digits.parse::<u64>().ok().map(|n| n * mult)
}

/// Classify a requested inclusive range `[start, end]` for write safety.
///
/// `reserved` and `system_ram` are inclusive `[start, end]` byte ranges (as
/// produced by [`parse_memmap_reserved`] and
/// [`crate::sysmem::system_ram_ranges`]). Precedence: fully inside a reserved
/// region wins; otherwise any overlap with System RAM is [`Safety::SystemRam`];
/// anything else is [`Safety::FirmwareOrMmio`].
#[must_use]
pub fn classify(
    start: u64,
    end: u64,
    reserved: &[(u64, u64)],
    system_ram: &[(u64, u64)],
) -> Safety {
    let fully_inside = |ranges: &[(u64, u64)]| ranges.iter().any(|&(s, e)| start >= s && end <= e);
    let overlaps = |ranges: &[(u64, u64)]| ranges.iter().any(|&(s, e)| start <= e && end >= s);

    if fully_inside(reserved) {
        Safety::Reserved
    } else if overlaps(system_ram) {
        Safety::SystemRam
    } else {
        Safety::FirmwareOrMmio
    }
}

/// Whether a write to a range of the given [`Safety`] is permitted, given the
/// `--devmem-unsafe` override.
#[must_use]
pub const fn write_allowed(safety: Safety, unsafe_override: bool) -> bool {
    match safety {
        Safety::Reserved => true,
        Safety::SystemRam => unsafe_override,
        Safety::FirmwareOrMmio => false,
    }
}

/// Turn a [`DevMemTarget`] into the concrete list of physical ranges to map,
/// each carrying its write-safety verdict.
///
/// `cmdline` is `/proc/cmdline`; `system_ram` is the inclusive `[start, end]`
/// System RAM ranges from [`crate::sysmem::system_ram_ranges`]. An explicit
/// range yields one mapping (safety classified); `reserved` yields one per
/// `memmap=` reservation (all [`Safety::Reserved`]).
///
/// # Errors
///
/// [`DevMemError::Unaligned`] if an explicit range is not page-aligned;
/// [`DevMemError::NoReservedRegions`] if `reserved` finds no reservations.
pub fn resolve_mappings(
    target: DevMemTarget,
    cmdline: &str,
    system_ram: &[(u64, u64)],
) -> Result<Vec<Mapping>, DevMemError> {
    let reserved = parse_memmap_reserved(cmdline);
    match target {
        DevMemTarget::Range { start, end } => {
            let len = (end - start + 1) as usize;
            if !start.is_multiple_of(4096) || !len.is_multiple_of(4096) {
                return Err(DevMemError::Unaligned { start, len });
            }
            let safety = classify(start, end, &reserved, system_ram);
            Ok(vec![Mapping {
                phys_start: start,
                len,
                safety,
            }])
        }
        DevMemTarget::Reserved => {
            if reserved.is_empty() {
                return Err(DevMemError::NoReservedRegions);
            }
            Ok(reserved
                .iter()
                .map(|&(s, e)| Mapping {
                    phys_start: s,
                    len: (e - s + 1) as usize,
                    safety: Safety::Reserved,
                })
                .collect())
        }
    }
}

/// Summarize a byte chunk `pread` from `/dev/mem` as whole 64-bit words.
///
/// Counts how many words were nonzero and folds an XOR signature. Trailing
/// bytes past the last whole word are ignored (physical ranges are
/// page-aligned, so there are none in practice).
///
/// `pread` is the safe way to read live System RAM: unlike `mmap`, it is not
/// blocked by the direct-map memtype conflict, and reading never corrupts.
/// Live RAM mutates under the read, so the checksum is a reachability signal,
/// not a stable value.
#[must_use]
pub fn probe_bytes(bytes: &[u8]) -> ProbeStats {
    let mut stats = ProbeStats::default();
    for chunk in bytes.chunks_exact(8) {
        let word = u64::from_le_bytes(chunk.try_into().unwrap_or([0; 8]));
        stats.words_read += 1;
        if word != 0 {
            stats.nonzero_words += 1;
        }
        stats.xor_checksum ^= word;
    }
    stats
}

/// A trivial [`PhysResolver`] for `/dev/mem` mappings.
///
/// The physical address of any virtual address is known exactly,
/// `phys_base + (vaddr - virt_base)`, with no pagemap lookup. The mapping is
/// fixed, so it is always stable.
pub struct DevMemResolver {
    virt_base: usize,
    phys_base: u64,
    len: usize,
}

impl DevMemResolver {
    #[must_use]
    pub const fn new(virt_base: usize, phys_base: u64, len: usize) -> Self {
        Self {
            virt_base,
            phys_base,
            len,
        }
    }
}

impl PhysResolver for DevMemResolver {
    fn build_map(&mut self, base: usize, len: usize) -> Result<MapStats, PhysError> {
        self.virt_base = base;
        self.len = len;
        let pages = len / 4096;
        Ok(MapStats {
            total_pages: pages,
            resolved_pages: pages,
            huge_pages: 0,
            thp_pages: 0,
            hwpoison_pages: 0,
            unevictable_pages: 0,
        })
    }

    fn resolve(&self, vaddr: usize) -> Result<PhysAddr, PhysError> {
        if vaddr < self.virt_base || vaddr >= self.virt_base + self.len {
            return Err(PhysError::PageNotPresent(vaddr));
        }
        Ok(PhysAddr(self.phys_base + (vaddr - self.virt_base) as u64))
    }

    fn page_flags(&self, _pfn: u64) -> Result<PageFlags, PhysError> {
        // kpageflags is meaningless for a direct physical mapping.
        Err(PhysError::PfnUnavailable(0))
    }

    fn verify_stability(&self, _base: usize, _len: usize) -> Result<usize, PhysError> {
        // A /dev/mem mapping points at fixed physical frames; nothing moves.
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod parse_target {
        use assert2::{assert, check};

        use super::*;

        #[test]
        fn reserved_keyword() {
            check!(parse_target("reserved") == Ok(DevMemTarget::Reserved));
        }

        #[test]
        fn hex_range() {
            assert!(let
                Ok(DevMemTarget::Range { start, end }) = parse_target("0x39400000-0x395fffff")
            );
            check!(start == 0x3940_0000);
            check!(end == 0x395f_ffff);
        }

        #[test]
        fn bare_hex_without_prefix() {
            assert!(let Ok(DevMemTarget::Range { start, end }) = parse_target("39400000-395fffff"));
            check!(start == 0x3940_0000);
            check!(end == 0x395f_ffff);
        }

        #[test]
        fn end_before_start_is_rejected() {
            check!(parse_target("0x2000-0x1000").is_err());
        }

        #[test]
        fn missing_separator_is_rejected() {
            check!(parse_target("0x39400000").is_err());
        }

        #[test]
        fn garbage_is_rejected() {
            check!(parse_target("not-a-range").is_err());
        }
    }

    mod memmap {
        use assert2::check;

        use super::*;

        #[test]
        fn parses_reserve_form_with_suffix() {
            let cmdline = "BOOT_IMAGE=/bzimage memmap=2M$0x39400000 quiet";
            check!(parse_memmap_reserved(cmdline) == vec![(0x3940_0000, 0x395f_ffff)]);
        }

        #[test]
        fn parses_multiple_reservations() {
            let cmdline = "memmap=2M$0x39400000 memmap=2M$0x673C00000 memmap=2M$0x7DAE00000";
            check!(
                parse_memmap_reserved(cmdline)
                    == vec![
                        (0x3940_0000, 0x395f_ffff),
                        (0x0006_73C0_0000, 0x0006_73DF_FFFF),
                        (0x0007_DAE0_0000, 0x0007_DAFF_FFFF),
                    ]
            );
        }

        #[test]
        fn ignores_non_reserve_forms() {
            // @ = usable, # = ACPI, ! = mark -- none are safe-to-write reservations.
            let cmdline = "memmap=1G@0x100000000 memmap=4M#0x50000000 memmap=1M!0x60000000";
            check!(parse_memmap_reserved(cmdline).is_empty());
        }

        #[test]
        fn empty_cmdline_yields_nothing() {
            check!(parse_memmap_reserved("").is_empty());
        }

        #[test]
        fn decimal_size_and_addr() {
            // 2097152 bytes = 2 MiB, addr 0 -> [0, 0x1fffff].
            check!(parse_memmap_reserved("memmap=2097152$0") == vec![(0, 0x1f_ffff)]);
        }
    }

    mod classify {
        use assert2::check;

        use super::*;

        const RESERVED: &[(u64, u64)] = &[(0x3940_0000, 0x395f_ffff)];
        // Mirrors roman: a big System RAM span around the reserved hole.
        const RAM: &[(u64, u64)] = &[(0x0010_0000, 0x393f_ffff), (0x3960_0000, 0xc349_7fff)];

        #[test]
        fn fully_inside_reserved_is_safe() {
            check!(classify(0x3940_0000, 0x394f_ffff, RESERVED, RAM) == Safety::Reserved);
        }

        #[test]
        fn inside_system_ram_needs_override() {
            // The known-bad neighborhood on roman sits in this span.
            check!(classify(0x5e52_6000, 0x5e52_6fff, RESERVED, RAM) == Safety::SystemRam);
        }

        #[test]
        fn overlapping_reserved_and_ram_is_system_ram() {
            // Straddles the reserved region's upper edge into System RAM.
            check!(classify(0x3950_0000, 0x3960_0fff, RESERVED, RAM) == Safety::SystemRam);
        }

        #[test]
        fn neither_reserved_nor_ram_is_firmware() {
            // Local APIC MMIO region, well above the RAM spans.
            check!(classify(0xfec0_0000, 0xfec0_0fff, RESERVED, RAM) == Safety::FirmwareOrMmio);
        }

        #[test]
        fn reserved_takes_precedence_over_ram_overlap() {
            // A reserved region nested inside a RAM span still classifies safe
            // when the request is fully within the reserved bounds.
            let ram = &[(0x0, 0xffff_ffff)];
            check!(classify(0x3940_0000, 0x395f_ffff, RESERVED, ram) == Safety::Reserved);
        }
    }

    mod write_allowed {
        use assert2::check;

        use super::*;

        #[test]
        fn reserved_always_writable() {
            check!(write_allowed(Safety::Reserved, false));
            check!(write_allowed(Safety::Reserved, true));
        }

        #[test]
        fn system_ram_only_with_override() {
            check!(!write_allowed(Safety::SystemRam, false));
            check!(write_allowed(Safety::SystemRam, true));
        }

        #[test]
        fn firmware_never_writable() {
            check!(!write_allowed(Safety::FirmwareOrMmio, false));
            check!(!write_allowed(Safety::FirmwareOrMmio, true));
        }
    }

    mod resolve_mappings {
        use assert2::{assert, check};

        use super::*;

        const RAM: &[(u64, u64)] = &[(0x0010_0000, 0xc349_7fff)];
        const CMDLINE: &str = "BOOT_IMAGE=/bzimage memmap=2M$0x39400000 memmap=2M$0x673C00000";

        #[test]
        fn explicit_range_in_system_ram() {
            let target = DevMemTarget::Range {
                start: 0x5e52_6000,
                end: 0x5e52_6fff,
            };
            assert!(let Ok(maps) = resolve_mappings(target, CMDLINE, RAM));
            check!(maps.len() == 1);
            check!(maps[0].phys_start == 0x5e52_6000);
            check!(maps[0].len == 0x1000);
            check!(maps[0].safety == Safety::SystemRam);
        }

        #[test]
        fn explicit_range_in_reserved_region() {
            let target = DevMemTarget::Range {
                start: 0x3940_0000,
                end: 0x394f_ffff,
            };
            assert!(let Ok(maps) = resolve_mappings(target, CMDLINE, RAM));
            check!(maps[0].safety == Safety::Reserved);
        }

        #[test]
        fn unaligned_range_is_rejected() {
            let target = DevMemTarget::Range {
                start: 0x1001,
                end: 0x2000,
            };
            check!(
                resolve_mappings(target, CMDLINE, RAM)
                    == Err(DevMemError::Unaligned {
                        start: 0x1001,
                        len: 0x1000,
                    })
            );
        }

        #[test]
        fn reserved_yields_one_mapping_per_reservation() {
            assert!(let Ok(maps) = resolve_mappings(DevMemTarget::Reserved, CMDLINE, RAM));
            check!(maps.len() == 2);
            check!(maps.iter().all(|m| m.safety == Safety::Reserved));
            check!(maps[0].phys_start == 0x3940_0000);
            check!(maps[0].len == 0x0020_0000);
            check!(maps[1].phys_start == 0x0006_73C0_0000);
        }

        #[test]
        fn reserved_without_reservations_errors() {
            check!(
                resolve_mappings(DevMemTarget::Reserved, "quiet", RAM)
                    == Err(DevMemError::NoReservedRegions)
            );
        }
    }

    mod probe_bytes {
        use assert2::check;

        use super::*;

        #[test]
        fn counts_nonzero_and_xors_le_words() {
            // Four little-endian u64 words: 0, 0xff, 0x0f, 0.
            let mut bytes = Vec::new();
            for w in [0u64, 0xff, 0x0f, 0] {
                bytes.extend_from_slice(&w.to_le_bytes());
            }
            let stats = probe_bytes(&bytes);
            check!(stats.words_read == 4);
            check!(stats.nonzero_words == 2);
            check!(stats.xor_checksum == 0xff ^ 0x0f);
        }

        #[test]
        fn ignores_trailing_partial_word() {
            let bytes = [0xffu8; 12]; // one whole word + 4 trailing bytes
            let stats = probe_bytes(&bytes);
            check!(stats.words_read == 1);
            check!(stats.xor_checksum == u64::MAX);
        }

        #[test]
        fn empty_range() {
            let stats = probe_bytes(&[]);
            check!(stats.words_read == 0);
            check!(stats.nonzero_words == 0);
            check!(stats.xor_checksum == 0);
        }

        #[test]
        fn merge_combines_chunks() {
            let a = probe_bytes(&1u64.to_le_bytes());
            let b = probe_bytes(&2u64.to_le_bytes());
            let merged = a.merge(b);
            check!(merged.words_read == 2);
            check!(merged.nonzero_words == 2);
            check!(merged.xor_checksum == 3);
        }
    }

    mod resolver {
        use assert2::{assert, check};

        use super::*;

        #[test]
        fn resolves_linearly_from_phys_base() {
            let r = DevMemResolver::new(0x1000, 0x3940_0000, 0x2000);
            assert!(let Ok(PhysAddr(p)) = r.resolve(0x1000));
            check!(p == 0x3940_0000);
            assert!(let Ok(PhysAddr(p2)) = r.resolve(0x1040));
            check!(p2 == 0x3940_0040);
        }

        #[test]
        fn out_of_range_fails() {
            let r = DevMemResolver::new(0x1000, 0x3940_0000, 0x2000);
            check!(r.resolve(0x900).is_err());
            check!(r.resolve(0x3000).is_err());
        }

        #[test]
        fn stability_is_always_zero() {
            let r = DevMemResolver::new(0x1000, 0x3940_0000, 0x2000);
            assert!(let Ok(changed) = r.verify_stability(0x1000, 0x2000));
            check!(changed == 0);
        }
    }
}
