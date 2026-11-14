use core::sync::atomic::{AtomicUsize, Ordering};

use assert2::check;
use rstest::{fixture, rstest};

use super::*;

#[derive(Default)]
struct Counting {
    runs: AtomicUsize,
    finishes: AtomicUsize,
}

// SAFETY: atomics only.
unsafe impl ApTask for Counting {
    fn run(&self) {
        self.runs.fetch_add(1, Ordering::Relaxed);
    }

    fn finish(&self, _stack: StackUse) {
        self.finishes.fetch_add(1, Ordering::Relaxed);
    }
}

const STACK: StackUse = StackUse { bytes: 0 };

#[fixture]
fn task() -> Counting {
    Counting::default()
}

#[rstest]
fn a_dispatch_that_runs_is_not_cancelled(task: Counting) {
    let tether = Tether::new(&task);
    tether.run();
    tether.finish(STACK);
    check!(task.runs.load(Ordering::Relaxed) == 1);
    check!(task.finishes.load(Ordering::Relaxed) == 1);
    check!(!tether.cancel(), "an AP already in the task cannot be called off");
}

#[rstest]
fn a_cancelled_dispatch_never_touches_the_task_even_when_the_ap_arrives_late(task: Counting) {
    let tether = Tether::new(&task);
    check!(tether.cancel());
    check!(tether.cancel(), "cancelling twice still says the task is safe");
    tether.run();
    tether.finish(STACK);
    check!(task.runs.load(Ordering::Relaxed) == 0);
    check!(task.finishes.load(Ordering::Relaxed) == 0);
}

#[test]
fn a_late_ap_can_outlive_the_task() {
    let tether = {
        let task = Counting::default();
        let tether = Tether::new(&task);
        check!(tether.cancel());
        tether
    };
    tether.run();
    tether.finish(STACK);
}
