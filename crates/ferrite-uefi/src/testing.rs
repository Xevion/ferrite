//! Test support shared by the modules: the test binary's allocator, counting the heap allocations
//! (and reallocations) of each thread.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::Cell;
use std::alloc::System;

std::thread_local! {
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
    static BYTES: Cell<u64> = const { Cell::new(0) };
}

struct Counting;

fn bump(bytes: usize) {
    // The keys are gone only while the thread is being torn down, when nothing is being counted.
    let _ = ALLOCS.try_with(|n| n.set(n.get() + 1));
    let _ = BYTES.try_with(|n| n.set(n.get() + bytes as u64));
}

// SAFETY: every method forwards to the system allocator with the arguments it was given; the
// counters are thread-locals that need no allocation.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        bump(layout.size());
        // SAFETY: the caller's contract is passed through unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        bump(layout.size());
        // SAFETY: the caller's contract is passed through unchanged.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract is passed through unchanged.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        bump(new_size);
        // SAFETY: the caller's contract is passed through unchanged.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Allocations and bytes requested so far by this thread.
pub fn allocations() -> (u64, u64) {
    (ALLOCS.with(Cell::get), BYTES.with(Cell::get))
}

/// Run `f` and return its result with the number of allocations it made.
pub fn count<R>(f: impl FnOnce() -> R) -> (R, u64) {
    let before = allocations().0;
    let out = f();
    (out, allocations().0 - before)
}
