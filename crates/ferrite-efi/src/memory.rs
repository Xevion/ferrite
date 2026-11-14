//! Memory from the firmware's boot services: whole pages claimed from the memory map, freed on drop.
//!
//! [`claim_free_memory`] takes every free range of the map that [`ranges::select`] picks, at the
//! ranges' own addresses; [`Claim::any_pages`] takes one block anywhere for when the map cannot be
//! read. Allocating moves the firmware's own pool, so the map is read again for what a range's
//! allocation could not get. A claim can lend its lowest pages back to the firmware
//! ([`Claim::lend_lowest`]), and [`claim_ranges`] takes ranges of the current map, which is how the
//! sweep tests the reserve it left free.

use alloc::vec::Vec;
use core::ptr::NonNull;

use ferrite_hal::PAGE_SIZE;
use uefi::boot::{self, AllocateType, MemoryType};
use uefi::mem::memory_map::{MemoryAttribute, MemoryMap, MemoryMapOwned};

use self::ranges::Range;

pub mod ranges;

const WORD: usize = size_of::<u64>();
/// How many times a claim reads the memory map for ranges its own allocations disturbed.
const CLAIM_ROUNDS: usize = 3;

/// One block of memory: boot-services pages, or a heap vector where there are no boot services.
#[derive(Debug)]
pub struct Block {
    base: NonNull<u64>,
    words: usize,
    owner: Owner,
}

#[derive(Debug)]
enum Owner {
    Pages(usize),
    Heap { _backing: Vec<u64> },
}

impl Block {
    fn pages(base: NonNull<u8>, pages: usize) -> Self {
        Self { base: base.cast(), words: pages * PAGE_SIZE / WORD, owner: Owner::Pages(pages) }
    }

    /// Free the first `pages` pages of a block of boot-services pages and keep the rest; the range
    /// freed. `None` for a heap block or when `pages` is not a proper part of the block.
    fn release_front(&mut self, pages: usize) -> Option<Range> {
        let &Owner::Pages(have) = &self.owner else { return None };
        if pages == 0 || pages >= have {
            return None;
        }
        let front = Self { base: self.base, words: pages * PAGE_SIZE / WORD, owner: Owner::Pages(pages) };
        let freed = Range::new(self.base.addr().get() as u64, pages);
        // SAFETY: `front.words` is less than the block's words, so the new base is inside it.
        self.base = unsafe { self.base.add(front.words) };
        self.words -= front.words;
        self.owner = Owner::Pages(have - pages);
        drop(front);
        Some(freed)
    }

    /// A block of `words` words on the heap, aligned to a cache line.
    #[must_use]
    pub fn heap(words: usize) -> Self {
        let mut backing = alloc::vec![0_u64; words + 8];
        let offset = backing.as_ptr().align_offset(64);
        let base = NonNull::from(&mut backing[offset]);
        Self { base, words, owner: Owner::Heap { _backing: backing } }
    }

    #[must_use]
    pub const fn base(&self) -> NonNull<u64> {
        self.base
    }

    #[must_use]
    pub const fn words(&self) -> usize {
        self.words
    }

    #[must_use]
    pub const fn bytes(&self) -> usize {
        self.words * WORD
    }
}

impl Drop for Block {
    fn drop(&mut self) {
        match self.owner {
            Owner::Pages(pages) => {
                // SAFETY: allocated with this page count, and the caller's views over it are gone
                // (a block's owner drops them first).
                if let Err(e) = unsafe { boot::free_pages(self.base.cast(), pages) } {
                    log::warn!("sweep: freeing {pages} pages at {:#x} failed: {e}", self.base.addr());
                }
            },
            Owner::Heap { .. } => {},
        }
    }
}

/// The memory a caller owns, in address order.
#[derive(Debug)]
pub struct Claim {
    blocks: Vec<Block>,
    /// The conventional bytes the memory map reported when the claim was made; 0 when unknown.
    reported_free: u64,
    /// The bytes [`claim_free_memory`] left free; 0 when unknown.
    reserve: u64,
}

impl Claim {
    #[must_use]
    pub const fn new(blocks: Vec<Block>) -> Self {
        Self { blocks, reported_free: 0, reserve: 0 }
    }

    /// One block of up to `bytes` anywhere, for when the memory map cannot be used.
    #[must_use]
    pub fn any_pages(bytes: usize) -> Option<Self> {
        let pages = bytes / PAGE_SIZE;
        let ptr = boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, pages).ok()?;
        Some(Self::new(alloc::vec![Block::pages(ptr, pages)]))
    }

    #[must_use]
    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    #[must_use]
    pub fn bytes(&self) -> usize {
        self.blocks.iter().map(Block::bytes).sum()
    }

    /// The conventional bytes the memory map reported free when this claim was made; 0 if unknown.
    #[must_use]
    pub const fn reported_free(&self) -> u64 {
        self.reported_free
    }

    /// The bytes [`claim_free_memory`] left free for the firmware; 0 if unknown.
    #[must_use]
    pub const fn reserve(&self) -> u64 {
        self.reserve
    }

    /// Give the firmware back the lowest `bytes` (whole pages) of the claim, cutting the block they
    /// are in if need be. The ranges freed; fewer bytes when the claim holds fewer pages that can
    /// be freed. The caller must hold no view of them any more.
    pub fn lend_lowest(&mut self, bytes: u64) -> Vec<Range> {
        let mut pages_left = (bytes / PAGE_SIZE as u64) as usize;
        let mut freed = Vec::new();
        let mut i = 0;
        while pages_left > 0 && i < self.blocks.len() {
            let Owner::Pages(have) = self.blocks[i].owner else {
                i += 1;
                continue;
            };
            if pages_left >= have {
                let block = self.blocks.remove(i);
                freed.push(Range::new(block.base.addr().get() as u64, have));
                pages_left -= have;
                drop(block);
            } else {
                freed.extend(self.blocks[i].release_front(pages_left));
                pages_left = 0;
            }
        }
        freed
    }

    /// Add `other`'s blocks, keeping address order.
    pub fn absorb(&mut self, other: Self) {
        self.blocks.extend(other.blocks);
        self.blocks.sort_by_key(|b| b.base.addr());
    }
}

/// The free ranges of the memory map that may be claimed, and the conventional memory it holds
/// in all.
struct FreeMemory {
    ranges: Vec<Range>,
    /// Every conventional range, claimable or not.
    all: Vec<Range>,
    conventional: u64,
}

impl FreeMemory {
    /// The conventional ranges of `map` that are cacheable and not set aside for a special purpose.
    fn of(map: &MemoryMapOwned) -> Self {
        let mut free = Self { ranges: Vec::new(), all: Vec::new(), conventional: 0 };
        for d in map.entries().filter(|d| d.ty == MemoryType::CONVENTIONAL) {
            free.conventional += d.page_count * PAGE_SIZE as u64;
            free.all.push(Range::new(d.phys_start, d.page_count as usize));
            let usable =
                d.att.contains(MemoryAttribute::WRITE_BACK) && !d.att.contains(MemoryAttribute::SPECIAL_PURPOSE);
            if usable {
                free.ranges.push(Range::new(d.phys_start, d.page_count as usize));
            }
        }
        free
    }
}

/// `part` of `whole` as a percentage, 0 to 100; 0 for an empty whole.
#[must_use]
pub const fn percent(part: u64, whole: u64) -> u64 {
    if whole == 0 { 0 } else { (part as u128 * 100 / whole as u128) as u64 }
}

/// What the memory map says is free, and what the firmware holds low down.
#[derive(Debug)]
pub struct FreeMap {
    ranges: Vec<Range>,
    all: Vec<Range>,
    conventional: u64,
    held: Vec<(MemoryType, u64)>,
}

impl FreeMap {
    /// The cacheable free ranges that may be claimed.
    #[must_use]
    pub fn ranges(&self) -> &[Range] {
        &self.ranges
    }

    /// Every conventional range, whatever its attributes: where pages the sweep freed itself are
    /// found again.
    #[must_use]
    pub fn all_ranges(&self) -> &[Range] {
        &self.all
    }

    /// Every conventional byte, claimable or not: what is free for the firmware's own allocations.
    #[must_use]
    pub const fn conventional(&self) -> u64 {
        self.conventional
    }

    /// Bytes of each memory type that is not free in `LOW_MEMORY..below`, as passed to
    /// [`free_map`]: the pages the firmware holds there.
    #[must_use]
    pub fn held(&self) -> &[(MemoryType, u64)] {
        &self.held
    }
}

/// Read the memory map; `None` when the firmware will not give it. `below` bounds the range
/// [`FreeMap::held`] covers; 0 for none.
#[must_use]
pub fn free_map(below: u64) -> Option<FreeMap> {
    let map =
        boot::memory_map(MemoryType::LOADER_DATA).inspect_err(|e| log::warn!("sweep: no memory map: {e}")).ok()?;
    let free = FreeMemory::of(&map);
    let held = ranges::held_below(
        map.entries()
            .filter(|d| d.ty != MemoryType::CONVENTIONAL)
            .map(|d| (d.ty, Range::new(d.phys_start, d.page_count as usize))),
        below,
    );
    Some(FreeMap { ranges: free.ranges, all: free.all, conventional: free.conventional, held })
}

/// Take whatever of `wanted` the firmware will still give, each range at its own address.
#[must_use]
pub fn claim_ranges(wanted: &[Range]) -> Claim {
    let mut blocks = Vec::new();
    take(&mut blocks, wanted, log::Level::Debug);
    blocks.sort_by_key(|b| b.base.addr());
    Claim::new(blocks)
}

/// Allocate each of `wanted` at its address, logging the ones taken at `level`.
fn take(blocks: &mut Vec<Block>, wanted: &[Range], level: log::Level) {
    for range in wanted {
        match boot::allocate_pages(AllocateType::Address(range.start.get()), MemoryType::LOADER_DATA, range.pages.get())
        {
            Ok(ptr) => {
                log::log!(level, "sweep: range {range}");
                blocks.push(Block::pages(ptr, range.pages.get()));
            },
            Err(e) => log::debug!("sweep: range {range} not taken: {e}"),
        }
    }
}

/// Claim every free range of the memory map except a reserve; `None` when no range could be taken.
///
/// The ranges are allocated one by one at their own addresses, the largest first. `reserve_for`
/// gives the bytes to leave free from the free bytes at or above [`ranges::LOW_MEMORY`], asked once.
pub fn claim_free_memory(reserve_for: impl Fn(u64) -> u64) -> Option<Claim> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut plan: Option<(u64, u64)> = None;
    for _ in 0..CLAIM_ROUNDS {
        // A later round that cannot read the map keeps what the earlier ones took.
        let Ok(map) =
            boot::memory_map(MemoryType::LOADER_DATA).inspect_err(|e| log::warn!("sweep: no memory map: {e}"))
        else {
            break;
        };
        let free = FreeMemory::of(&map);
        drop(map);
        let (_, reserve) = *plan.get_or_insert_with(|| {
            let reserve = reserve_for(ranges::eligible_bytes(&free.ranges));
            (free.conventional, reserve)
        });
        let wanted = ranges::select(&free.ranges, reserve);
        let before = blocks.len();
        take(&mut blocks, &wanted, log::Level::Info);
        if blocks.len() == before {
            break;
        }
    }
    blocks.sort_by_key(|b| b.base.addr());
    let (conventional, reserve) = plan?;
    let claim = Claim { blocks, reported_free: conventional, reserve };
    log::info!(
        "sweep: {} ranges, {} MiB of the {} MiB the memory map reports free ({}%); the {} MiB reserve is tested in a rotation after each pass, memory below {} MiB never",
        claim.blocks.len(),
        claim.bytes() >> 20,
        conventional >> 20,
        percent(claim.bytes() as u64, conventional),
        reserve >> 20,
        ranges::LOW_MEMORY >> 20
    );
    (!claim.blocks.is_empty()).then_some(claim)
}
