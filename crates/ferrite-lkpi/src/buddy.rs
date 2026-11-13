//! A binary buddy allocator over a range of page frames, orders 0 to [`MAX_ORDER`].
//!
//! Blocks are aligned by their absolute frame number, so a block of order `k` starts on a frame that
//! is a multiple of `2^k` even when the range itself does not. The allocator tracks only frame
//! numbers; the caller owns the memory and its `struct page`s.

use alloc::vec;
use alloc::vec::Vec;

use snafu::Snafu;

/// Linux's `MAX_PAGE_ORDER`: the largest block is 1024 pages.
pub const MAX_ORDER: u8 = 10;

const NONE: u32 = u32::MAX;
/// State byte of a page that starts no block: inside a block, or reserved.
const COVERED: u8 = 0;
const FREE: u8 = 0x80;
const ALLOCATED: u8 = 0x40;
const ORDER_MASK: u8 = 0x0f;

#[derive(Debug, Snafu, PartialEq, Eq)]
pub enum FreeError {
    /// The page starts no allocated block: a double free, or a page never handed out.
    #[snafu(display("page {index} is not the start of an allocated block"))]
    NotAllocated { index: usize },
    /// The page starts an allocated block of another order.
    #[snafu(display("page {index} starts a block of order {allocated}, freed as order {freed}"))]
    OrderMismatch { index: usize, allocated: u8, freed: u8 },
}

#[derive(Debug, Snafu, PartialEq, Eq)]
#[snafu(display("a range of {pages} pages from frame {base} does not fit the allocator's 32-bit indices"))]
pub struct RangeTooLarge {
    pub base: usize,
    pub pages: usize,
}

pub struct Buddy {
    /// Absolute frame number of index 0.
    base: usize,
    /// Per page: `FREE | order` or `ALLOCATED | order` on a block's first page, else `COVERED`.
    state: Vec<u8>,
    next: Vec<u32>,
    prev: Vec<u32>,
    heads: [u32; MAX_ORDER as usize + 1],
    free_pages: usize,
}

impl Buddy {
    /// Pages `reserved..pages` of the range starting at frame `base` become free; the first
    /// `reserved` are never handed out.
    pub fn new(base: usize, pages: usize, reserved: usize) -> Result<Self, RangeTooLarge> {
        if u32::try_from(pages).is_err() || pages == NONE as usize || base.checked_add(pages).is_none() {
            return Err(RangeTooLarge { base, pages });
        }
        let mut buddy = Self {
            base,
            state: vec![COVERED; pages],
            next: vec![NONE; pages],
            prev: vec![NONE; pages],
            heads: [NONE; MAX_ORDER as usize + 1],
            free_pages: 0,
        };
        let mut index = reserved.min(pages);
        while index < pages {
            let order = (0..=MAX_ORDER)
                .rev()
                .find(|&k| (base + index).is_multiple_of(1 << k) && index + (1 << k) <= pages)
                .unwrap_or(0);
            buddy.push(index, order);
            index += 1 << order;
        }
        Ok(buddy)
    }

    /// Pages currently free.
    pub const fn free_pages(&self) -> usize {
        self.free_pages
    }

    /// A block of `2^order` pages, as the index of its first page.
    pub fn alloc(&mut self, order: u8) -> Option<usize> {
        if order > MAX_ORDER {
            return None;
        }
        let found = (order..=MAX_ORDER).find(|&k| self.heads[usize::from(k)] != NONE)?;
        let index = self.heads[usize::from(found)] as usize;
        self.unlink(index, found);
        for k in (order..found).rev() {
            self.push(index + (1 << k), k);
        }
        self.state[index] = ALLOCATED | order;
        Some(index)
    }

    /// Return the block of `2^order` pages starting at `index`, merging it with free buddies.
    pub fn free(&mut self, index: usize, order: u8) -> Result<(), FreeError> {
        self.check_allocated(index, order)?;
        self.state[index] = COVERED;
        let (mut index, mut order) = (index, order);
        while order < MAX_ORDER {
            let buddy = ((self.base + index) ^ (1 << order)).wrapping_sub(self.base);
            if buddy >= self.state.len() || self.state[buddy] != FREE | order {
                break;
            }
            self.unlink(buddy, order);
            self.state[buddy] = COVERED;
            index = index.min(buddy);
            order += 1;
        }
        self.push(index, order);
        Ok(())
    }

    /// Turn the allocated block of `2^order` pages at `index` into that many allocated single pages,
    /// as Linux's `split_page` does, so each can be freed on its own.
    pub fn split(&mut self, index: usize, order: u8) -> Result<(), FreeError> {
        self.check_allocated(index, order)?;
        self.state[index..index + (1 << order)].fill(ALLOCATED);
        Ok(())
    }

    /// Whether the page at `index` lies inside a free block.
    pub fn is_free(&self, index: usize) -> bool {
        if index >= self.state.len() {
            return false;
        }
        (0..=MAX_ORDER).any(|k| {
            let head = ((self.base + index) & !((1 << k) - 1)).wrapping_sub(self.base);
            head < self.state.len() && self.state[head] & FREE != 0 && self.state[head] & ORDER_MASK >= k
        })
    }

    fn check_allocated(&self, index: usize, order: u8) -> Result<(), FreeError> {
        let state = *self.state.get(index).ok_or(FreeError::NotAllocated { index })?;
        if state & ALLOCATED == 0 {
            return Err(FreeError::NotAllocated { index });
        }
        let allocated = state & ORDER_MASK;
        if allocated != order {
            return Err(FreeError::OrderMismatch { index, allocated, freed: order });
        }
        Ok(())
    }

    #[expect(clippy::cast_possible_truncation, reason = "indices fit u32, checked in `new`")]
    fn push(&mut self, index: usize, order: u8) {
        let head = self.heads[usize::from(order)];
        self.state[index] = FREE | order;
        self.next[index] = head;
        self.prev[index] = NONE;
        if head != NONE {
            self.prev[head as usize] = index as u32;
        }
        self.heads[usize::from(order)] = index as u32;
        self.free_pages += 1 << order;
    }

    fn unlink(&mut self, index: usize, order: u8) {
        let (next, prev) = (self.next[index], self.prev[index]);
        if prev == NONE {
            self.heads[usize::from(order)] = next;
        } else {
            self.next[prev as usize] = next;
        }
        if next != NONE {
            self.prev[next as usize] = prev;
        }
        self.free_pages -= 1 << order;
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use assert2::{assert, check};
    use rstest::rstest;

    use super::*;

    #[test]
    fn splits_down_and_merges_back_to_one_block() {
        let mut buddy = Buddy::new(0, 1024, 0).unwrap();
        let a = buddy.alloc(0).unwrap();
        let b = buddy.alloc(0).unwrap();
        let c = buddy.alloc(3).unwrap();
        check!([a, b, c] == [0, 1, 8]);
        check!(buddy.free_pages() == 1024 - 10);
        for (index, order) in [(b, 0), (c, 3), (a, 0)] {
            buddy.free(index, order).unwrap();
        }
        check!(buddy.free_pages() == 1024);
        check!(buddy.alloc(MAX_ORDER) == Some(0));
    }

    #[rstest]
    #[case(0, 0)]
    #[case(3, 5)]
    #[case(0x40_0003, 37)]
    fn blocks_are_aligned_by_absolute_frame(#[case] base: usize, #[case] reserved: usize) {
        let mut buddy = Buddy::new(base, 4096, reserved).unwrap();
        for order in [0, 4, 2, MAX_ORDER, 7] {
            let index = buddy.alloc(order).unwrap();
            check!(index >= reserved);
            check!((base + index).is_multiple_of(1 << order), "order {order} at frame {}", base + index);
        }
    }

    #[test]
    fn an_order_runs_out_while_smaller_blocks_remain() {
        let mut buddy = Buddy::new(0, 2048, 1).unwrap();
        // Page 0 is reserved, so only the upper half holds an order-10 block.
        check!(buddy.alloc(MAX_ORDER) == Some(1024));
        check!(buddy.alloc(MAX_ORDER) == None);
        check!(buddy.alloc(MAX_ORDER + 1) == None);
        let small: Vec<_> = core::iter::from_fn(|| buddy.alloc(9)).collect();
        check!(small == [512]);
        check!(buddy.free_pages() == 511);
    }

    #[test]
    fn double_free_and_wrong_order_are_refused() {
        let mut buddy = Buddy::new(0, 64, 0).unwrap();
        let block = buddy.alloc(2).unwrap();
        check!(buddy.free(block, 1) == Err(FreeError::OrderMismatch { index: block, allocated: 2, freed: 1 }));
        check!(buddy.free(block + 1, 0) == Err(FreeError::NotAllocated { index: block + 1 }));
        buddy.free(block, 2).unwrap();
        check!(buddy.free(block, 2) == Err(FreeError::NotAllocated { index: block }));
        check!(buddy.free(1000, 0) == Err(FreeError::NotAllocated { index: 1000 }));
        check!(buddy.free_pages() == 64);
    }

    #[test]
    fn a_split_block_frees_page_by_page_and_remerges() {
        let mut buddy = Buddy::new(0, 16, 0).unwrap();
        let block = buddy.alloc(2).unwrap();
        buddy.split(block, 2).unwrap();
        check!(!buddy.is_free(block + 3));
        for page in (block..block + 4).rev() {
            buddy.free(page, 0).unwrap();
        }
        check!(buddy.is_free(block + 3));
        assert!(let Some(0) = buddy.alloc(4));
    }

    #[test]
    fn is_free_sees_pages_inside_free_blocks() {
        let mut buddy = Buddy::new(0, 32, 0).unwrap();
        let page = buddy.alloc(0).unwrap();
        check!(!buddy.is_free(page));
        check!(buddy.is_free(page + 1));
        check!(buddy.is_free(31));
        check!(!buddy.is_free(32));
    }
}
