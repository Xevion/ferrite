//! Which free ranges of the firmware's memory map a claim takes, and how much it leaves.

use alloc::vec::Vec;
use core::fmt;

use ferrite_hal::{PAGE_SIZE, PageCount, PhysAddr};

/// Memory below this is never claimed: the firmware keeps its real-mode and legacy areas
/// there, and the processors' start-up code may sit in it.
pub const LOW_MEMORY: u64 = 1 << 20;
/// Free ranges smaller than this are not worth a region of their own.
pub const MIN_RANGE_BYTES: u64 = 1 << 20;

/// A run of whole free pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
    pub start: PhysAddr,
    pub pages: PageCount,
}

impl Range {
    #[must_use]
    pub const fn new(start: u64, pages: usize) -> Self {
        Self { start: PhysAddr::new(start), pages: PageCount::new(pages) }
    }

    /// One past the last byte.
    #[must_use]
    pub const fn end(self) -> u64 {
        self.start.get() + self.pages.get() as u64 * PAGE_SIZE as u64
    }

    #[must_use]
    pub const fn bytes(self) -> u64 {
        self.pages.get() as u64 * PAGE_SIZE as u64
    }
}

impl fmt::Display for Range {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#x}..{:#x} ({} MiB)", self.start.get(), self.end(), self.bytes() >> 20)
    }
}

/// The free bytes at or above [`LOW_MEMORY`] in `ranges`.
#[must_use]
pub fn eligible_bytes(ranges: &[Range]) -> u64 {
    clipped(ranges).iter().map(|r| r.bytes()).sum()
}

/// The ranges to take out of `free`, largest first.
///
/// Every page is taken except the lowest `reserve` bytes at or above [`LOW_MEMORY`], in pieces of
/// at least [`MIN_RANGE_BYTES`]. Leaving the lowest addresses keeps what devices with narrow DMA need (the Wi-Fi arena, the firmware's own
/// buffers) available to the allocations that follow the sweep.
#[must_use]
pub fn select(free: &[Range], reserve: u64) -> Vec<Range> {
    let mut skip = reserve.next_multiple_of(PAGE_SIZE as u64);
    let mut taken: Vec<Range> = Vec::new();
    for range in clipped(free) {
        if skip >= range.bytes() {
            skip -= range.bytes();
            continue;
        }
        let start = range.start.get() + skip;
        skip = 0;
        let pages = ((range.end() - start) / PAGE_SIZE as u64) as usize;
        if pages as u64 * PAGE_SIZE as u64 >= MIN_RANGE_BYTES {
            taken.push(Range::new(start, pages));
        }
    }
    taken.sort_by_key(|r| core::cmp::Reverse(r.pages));
    taken
}

/// `ranges` cut at [`LOW_MEMORY`], in address order, with touching ranges joined into one.
fn clipped(ranges: &[Range]) -> Vec<Range> {
    let mut sorted: Vec<Range> = ranges
        .iter()
        .filter_map(|r| {
            let start = r.start.get().max(LOW_MEMORY);
            let pages = r.end().checked_sub(start)? / PAGE_SIZE as u64;
            (pages > 0).then(|| Range::new(start, pages as usize))
        })
        .collect();
    sorted.sort_by_key(|r| r.start);
    let mut joined: Vec<Range> = Vec::with_capacity(sorted.len());
    for range in sorted {
        match joined.last_mut() {
            Some(last) if last.end() >= range.start.get() => {
                let end = last.end().max(range.end());
                *last = Range::new(last.start.get(), ((end - last.start.get()) / PAGE_SIZE as u64) as usize);
            },
            _ => joined.push(range),
        }
    }
    joined
}

#[cfg(test)]
mod tests;
