//! Physical-memory subsystem: address resolution, coverage tracking, gap
//! classification, frame-hostage culling, installed-RAM accounting, and
//! `/dev/mem` targeted testing.
//!
//! Everything here reasons about physical frames (PFNs) and byte-granular
//! physical addresses. The two are distinct units -- [`pfn::Pfn`] is the frame
//! number, [`phys::PhysAddr`] the byte address -- and conversions between them
//! go through [`pfn::Pfn::to_addr`] / [`pfn::Pfn::from_addr`].

pub mod coverage;
pub mod devmem;
pub mod gap;
pub mod kpageflags;
pub mod lifecycle;
pub mod pfn;
pub mod phys;
pub mod sieve;
pub mod sysmem;

pub use pfn::{Pfn, PfnRange};

/// Bytes per page frame (4 KiB), the granularity of every PFN-indexed kernel
/// interface (`/proc/self/pagemap`, `/proc/kpageflags`) and of coverage
/// tracking. Byte-address form.
pub const PAGE_BYTES: u64 = 4096;

/// [`PAGE_BYTES`] as `usize`, for slice and offset arithmetic.
pub const PAGE_BYTES_USIZE: usize = PAGE_BYTES as usize;

/// Parse `"START-END"` into a `(start, end)` pair of hexadecimal numbers.
///
/// `allow_prefix` tolerates a leading `0x`/`0X` on each number (as `--devmem`
/// arguments may carry); with it `false`, only bare hex is accepted (matching
/// the format `/proc/iomem` prints). No ordering relationship between `start`
/// and `end` is imposed -- callers apply their own validation and error style.
#[must_use]
pub fn parse_hex_range(s: &str, allow_prefix: bool) -> Option<(u64, u64)> {
    let (start, end) = s.split_once('-')?;
    let start = parse_hex(start.trim(), allow_prefix)?;
    let end = parse_hex(end.trim(), allow_prefix)?;
    Some((start, end))
}

/// Parse a single hexadecimal number, optionally tolerating a `0x`/`0X` prefix.
#[must_use]
pub fn parse_hex(s: &str, allow_prefix: bool) -> Option<u64> {
    let hex = if allow_prefix {
        s.strip_prefix("0x")
            .or_else(|| s.strip_prefix("0X"))
            .unwrap_or(s)
    } else {
        s
    };
    u64::from_str_radix(hex, 16).ok()
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    #[test]
    fn parses_bare_hex_range() {
        check!(parse_hex_range("00001000-00001fff", false) == Some((0x1000, 0x1fff)));
    }

    #[test]
    fn parses_prefixed_hex_range_when_allowed() {
        check!(parse_hex_range("0x39400000-0x395fffff", true) == Some((0x3940_0000, 0x395f_ffff)));
    }

    #[test]
    fn prefixed_range_rejected_without_allow_prefix() {
        check!(parse_hex_range("0x1000-0x2000", false) == None);
    }

    #[test]
    fn missing_dash_is_none() {
        check!(parse_hex_range("0x39400000", true) == None);
    }

    #[test]
    fn non_hex_is_none() {
        check!(parse_hex_range("zzzz-yyyy", false) == None);
    }

    #[test]
    fn order_is_not_validated() {
        // The helper parses without imposing start <= end.
        check!(parse_hex_range("2000-1000", false) == Some((0x2000, 0x1000)));
    }
}
