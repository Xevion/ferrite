//! A dispatch the BSP can call off while the AP has not begun it.
//!
//! The firmware starts an AP task whenever the AP gets to it, and an AP that never does keeps the
//! scope that dispatched it waiting until the dispatch's own timeout, minutes later. The task the
//! firmware is handed is a [`Tether`] on the heap instead of the caller's task: it runs the task only
//! if it wins the race against [`Tether::cancel`], so once a cancel has won, the task and everything
//! it borrows may go away and a late AP finds nothing to run.

use core::sync::atomic::{AtomicU8, Ordering};

use super::{ApTask, StackUse};

const PENDING: u8 = 0;
const ENTERED: u8 = 1;
const CANCELLED: u8 = 2;

pub(super) struct Tether {
    state: AtomicU8,
    task: *const (),
    run: unsafe fn(*const ()),
    finish: unsafe fn(*const (), StackUse),
}

// SAFETY: `task` points at an `ApTask`, which is `Sync`, and is only dereferenced by the one
// caller that has won `state` from PENDING to ENTERED.
unsafe impl Sync for Tether {}

impl Tether {
    /// A tether on `task`. The caller keeps `task` alive until [`Tether::cancel`] has returned
    /// true or the AP has ended the dispatch.
    pub(super) fn new<T: ApTask>(task: &T) -> Self {
        unsafe fn run<T: ApTask>(task: *const ()) {
            // SAFETY: `Tether::new`'s caller keeps the task alive while a call can happen.
            unsafe { (*task.cast::<T>()).run() };
        }
        unsafe fn finish<T: ApTask>(task: *const (), stack: StackUse) {
            // SAFETY: as in `run`.
            unsafe { (*task.cast::<T>()).finish(stack) };
        }
        Self {
            state: AtomicU8::new(PENDING),
            task: core::ptr::from_ref(task).cast(),
            run: run::<T>,
            finish: finish::<T>,
        }
    }

    /// Call the dispatch off; true when the AP has not begun it and never will, so the task is no
    /// longer touched. False when the AP is already in the task, which must then be waited for.
    pub(super) fn cancel(&self) -> bool {
        matches!(
            self.state.compare_exchange(PENDING, CANCELLED, Ordering::AcqRel, Ordering::Acquire),
            Ok(_) | Err(CANCELLED)
        )
    }
}

// SAFETY: runs the tethered task, with its own contract, only when no cancel has won; the check
// itself is one atomic exchange.
unsafe impl ApTask for Tether {
    fn run(&self) {
        if self.state.compare_exchange(PENDING, ENTERED, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            // SAFETY: no cancel can win from here on, so the caller still holds the task.
            unsafe { (self.run)(self.task) };
        }
    }

    fn finish(&self, stack: StackUse) {
        if self.state.load(Ordering::Acquire) == ENTERED {
            // SAFETY: as in `run`.
            unsafe { (self.finish)(self.task, stack) };
        }
    }
}

#[cfg(test)]
mod tests;
