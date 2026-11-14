//! Memory from the firmware's boot services: whole pages claimed from the memory map, freed on drop.
//!
//! [`claim_free_memory`] takes every free range of the map that [`ranges::select`] picks, at the
//! ranges' own addresses; [`Claim::any_pages`] takes one block anywhere for when the map cannot be
//! read. Allocating moves the firmware's own pool, so the map is read again for what a range's
//! allocation could not get.

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
}

impl Claim {
    #[must_use]
    pub const fn new(blocks: Vec<Block>) -> Self {
        Self { blocks }
    }

    /// One block of up to `bytes` anywhere, for when the memory map cannot be used.
    #[must_use]
    pub fn any_pages(bytes: usize) -> Option<Self> {
        let pages = bytes / PAGE_SIZE;
        let ptr = boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, pages).ok()?;
        Some(Self { blocks: alloc::vec![Block::pages(ptr, pages)] })
    }

    #[must_use]
    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    #[must_use]
    pub fn bytes(&self) -> usize {
        self.blocks.iter().map(Block::bytes).sum()
    }
}

/// The free ranges of the memory map that may be claimed, and the conventional memory it holds
/// in all.
struct FreeMemory {
    ranges: Vec<Range>,
    conventional: u64,
}

impl FreeMemory {
    /// The conventional ranges of `map` that are cacheable and not set aside for a special purpose.
    fn of(map: &MemoryMapOwned) -> Self {
        let mut free = Self { ranges: Vec::new(), conventional: 0 };
        for d in map.entries().filter(|d| d.ty == MemoryType::CONVENTIONAL) {
            free.conventional += d.page_count * PAGE_SIZE as u64;
            let usable =
                d.att.contains(MemoryAttribute::WRITE_BACK) && !d.att.contains(MemoryAttribute::SPECIAL_PURPOSE);
            if usable {
                free.ranges.push(Range::new(d.phys_start, d.page_count as usize));
            }
        }
        free
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
        for range in wanted {
            match boot::allocate_pages(
                AllocateType::Address(range.start.get()),
                MemoryType::LOADER_DATA,
                range.pages.get(),
            ) {
                Ok(ptr) => {
                    log::info!("sweep: range {range}");
                    blocks.push(Block::pages(ptr, range.pages.get()));
                },
                Err(e) => log::debug!("sweep: range {range} not taken: {e}"),
            }
        }
        if blocks.len() == before {
            break;
        }
    }
    blocks.sort_by_key(|b| b.base.addr());
    let claim = Claim { blocks };
    let (conventional, reserve) = plan?;
    log::info!(
        "sweep: {} ranges, {} MiB of the {} MiB the memory map reports free; {} MiB reserve, memory below {} MiB untouched",
        claim.blocks.len(),
        claim.bytes() >> 20,
        conventional >> 20,
        reserve >> 20,
        ranges::LOW_MEMORY >> 20
    );
    (!claim.blocks.is_empty()).then_some(claim)
}
