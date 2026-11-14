//! Test support: a count of heap allocations on the calling thread, and a sample status.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::Cell;
use std::alloc::System;

use ferrite_mem::analysis::Classification;
use ferrite_mem::suite::{Mode, Preset};

use crate::sweep::SweepStatus;

std::thread_local! {
    static COUNT: Cell<u64> = const { Cell::new(0) };
}

struct Counting;

fn bump() {
    // The key is gone only while the thread is being torn down, when nothing is being counted.
    let _ = COUNT.try_with(|count| count.set(count.get() + 1));
}

// SAFETY: every method forwards to the system allocator with the arguments it was given.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        bump();
        // SAFETY: the caller's contract is passed through unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        bump();
        // SAFETY: the caller's contract is passed through unchanged.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract is passed through unchanged.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        bump();
        // SAFETY: the caller's contract is passed through unchanged.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Run `f` and return its result with the number of allocations (and reallocations) it made.
pub fn count<R>(f: impl FnOnce() -> R) -> (R, u64) {
    let before = COUNT.with(Cell::get);
    let out = f();
    (out, COUNT.with(Cell::get) - before)
}

/// A status for a sweep that has just started.
pub fn idle_status() -> SweepStatus {
    SweepStatus {
        bytes: 1 << 20,
        test: "own-address",
        position: (1, 9),
        test_percent: 0,
        percent: 0,
        passes: 0,
        tier: "sse2",
        mode: Mode::Single,
        cpus: 1,
        faults: 0,
        first_fault: None,
        analysis: Classification::Clean,
        preset: Preset::Standard,
        paused: false,
    }
}
