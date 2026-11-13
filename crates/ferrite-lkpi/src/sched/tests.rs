use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::hint::black_box;
use core::num::NonZeroUsize;
use core::time::Duration;
use std::alloc::{Layout, alloc_zeroed, dealloc};

use assert2::{assert, check};
use corosensei::stack::StackPointer;
use rstest::rstest;

use super::*;

fn at(ms: u64) -> Instant {
    Instant::from_epoch(Duration::from_millis(ms))
}

fn prio(level: u8) -> Priority {
    Priority::new(level).unwrap()
}

const HOUR: Duration = Duration::from_secs(3600);

struct TestStack {
    ptr: *mut u8,
    size: usize,
}

// SAFETY: the allocation is page aligned, `size` bytes, and owned by this value until `release`.
unsafe impl Stack for TestStack {
    fn base(&self) -> StackPointer {
        NonZeroUsize::new(self.ptr as usize + self.size).unwrap()
    }

    fn limit(&self) -> StackPointer {
        NonZeroUsize::new(self.ptr as usize).unwrap()
    }
}

#[derive(Clone, Default)]
struct TestSource {
    allocated: Rc<Cell<usize>>,
    released: Rc<Cell<usize>>,
    limits: Rc<RefCell<Vec<usize>>>,
    fail: Rc<Cell<bool>>,
}

impl TestSource {
    fn layout(size: usize) -> Layout {
        Layout::from_size_align(size, 4096).unwrap()
    }
}

impl StackSource for TestSource {
    type Stack = TestStack;

    fn allocate(&mut self, size: usize) -> Result<TestStack, StackAllocError> {
        if self.fail.get() {
            return Err(StackAllocError { size });
        }
        // SAFETY: the layout has a nonzero size.
        let ptr = unsafe { alloc_zeroed(Self::layout(size)) };
        self.allocated.set(self.allocated.get() + 1);
        self.limits.borrow_mut().push(ptr as usize);
        Ok(TestStack { ptr, size })
    }

    fn release(&mut self, stack: TestStack) {
        self.released.set(self.released.get() + 1);
        // SAFETY: `allocate` made this block with the same layout.
        unsafe { dealloc(stack.ptr, Self::layout(stack.size)) };
    }
}

#[derive(Clone, Default)]
struct Fake {
    clock: Rc<Cell<Instant>>,
    irq: Rc<Cell<bool>>,
    armed: Rc<RefCell<Vec<Option<Instant>>>>,
}

impl Platform for Fake {
    fn now(&self) -> Instant {
        self.clock.get()
    }

    fn arm_wakeup(&mut self, at: Option<Instant>) {
        self.armed.borrow_mut().push(at);
    }

    fn take_pending_irqs(&mut self) -> bool {
        self.irq.replace(false)
    }
}

#[derive(Clone, Default)]
struct Probe {
    entered: Rc<RefCell<Vec<Instant>>>,
    quiescent: Rc<Cell<u32>>,
    fail: Rc<Cell<bool>>,
}

impl Hooks for Probe {
    fn on_enter(&mut self, now: Instant) {
        self.entered.borrow_mut().push(now);
    }

    fn check(&self) -> Result<(), &'static str> {
        if self.fail.get() { Err("preempt_count") } else { Ok(()) }
    }

    fn quiescent(&mut self) {
        self.quiescent.set(self.quiescent.get() + 1);
    }
}

type Events = Rc<RefCell<Vec<&'static str>>>;
type TestSched = Scheduler<Fake, TestSource, Probe>;

struct Rig {
    sched: TestSched,
    fake: Fake,
    probe: Probe,
    src: TestSource,
    events: Events,
}

impl Rig {
    fn new() -> Self {
        let (fake, probe, src) = (Fake::default(), Probe::default(), TestSource::default());
        Self {
            sched: Scheduler::new(fake.clone(), src.clone(), probe.clone()),
            fake,
            probe,
            src,
            events: Events::default(),
        }
    }

    fn spawn(&mut self, name: &'static str, level: u8, body: impl FnOnce(&Task<'_>) + 'static) -> TaskId {
        self.sched.spawn(name, prio(level), 0, body).unwrap()
    }

    /// A body that records `name` once.
    fn note(&self, name: &'static str) -> impl FnOnce(&Task<'_>) + 'static {
        let events = Rc::clone(&self.events);
        move |_| events.borrow_mut().push(name)
    }

    fn poll(&mut self) -> Step {
        self.sched.poll(HOUR).unwrap()
    }

    fn events(&self) -> Vec<&'static str> {
        self.events.borrow().clone()
    }
}

#[rstest]
#[case(0, true)]
#[case(139, true)]
#[case(140, false)]
#[case(255, false)]
fn priority_accepts_only_linux_levels(#[case] level: u8, #[case] valid: bool) {
    check!(Priority::new(level).is_ok() == valid);
}

#[test]
fn best_priority_runs_first_and_keeps_the_cpu_while_it_yields() {
    let mut rig = Rig::new();
    let low = rig.note("low");
    rig.spawn("low", 100, low);
    let mid = rig.note("mid");
    rig.spawn("mid", 50, mid);
    let events = Rc::clone(&rig.events);
    rig.spawn("high", 10, move |t| {
        events.borrow_mut().push("high1");
        t.schedule(false);
        events.borrow_mut().push("high2");
    });

    let step = rig.poll();

    check!(rig.events() == ["high1", "high2", "mid", "low"]);
    check!(step.runnable == false);
}

#[test]
fn equal_priority_tasks_that_keep_yielding_alternate() {
    let mut rig = Rig::new();
    for name in ["a", "b"] {
        let events = Rc::clone(&rig.events);
        rig.spawn(name, 120, move |t| {
            for _ in 0..3 {
                events.borrow_mut().push(name);
                t.schedule(false);
            }
        });
    }

    rig.poll();

    check!(rig.events() == ["a", "b", "a", "b", "a", "b"]);
}

#[test]
fn blocked_task_waits_for_unblock_and_a_second_unblock_is_harmless() {
    let mut rig = Rig::new();
    let events = Rc::clone(&rig.events);
    let id = rig.spawn("sleeper", 120, move |t| {
        events.borrow_mut().push("before");
        t.schedule(true);
        events.borrow_mut().push("after");
    });

    rig.poll();
    check!(rig.events() == ["before"]);
    check!(rig.sched.info(id).unwrap().state == TaskState::Blocked);
    check!(rig.poll().ran == 0);

    check!(rig.sched.unblock(id) == Unblocked::Woken);
    check!(rig.sched.unblock(id) == Unblocked::AlreadyRunnable);
    rig.poll();
    check!(rig.events() == ["before", "after"]);
}

#[test]
fn stale_id_is_detected_after_its_slot_is_reused() {
    let mut rig = Rig::new();
    let body = rig.note("first");
    let first = rig.spawn("first", 120, body);
    rig.poll();
    check!(rig.sched.unblock(first) == Unblocked::Exited);

    let body = rig.note("second");
    let second = rig.spawn("second", 120, body);

    check!(first != second);
    check!(rig.sched.unblock(first) == Unblocked::Exited);
    check!(rig.sched.info(first) == None);
    check!(rig.sched.unblock(second) == Unblocked::AlreadyRunnable);
}

#[test]
fn exited_task_returns_its_stack_to_the_source() {
    let mut rig = Rig::new();
    let body = rig.note("t");
    rig.spawn("t", 120, body);
    rig.poll();
    check!(rig.src.allocated.get() == 1);
    check!(rig.src.released.get() == 1);
}

#[test]
fn find_by_user_follows_live_tasks() {
    let mut rig = Rig::new();
    let id = rig.sched.spawn("kworker", prio(120), 0xdead, |_| {}).unwrap();
    check!(rig.sched.find_by_user(0xdead) == Some(id));
    check!(rig.sched.find_by_user(0xbeef) == None);
    check!(rig.sched.info(id).unwrap().user == 0xdead);

    rig.poll();
    check!(rig.sched.find_by_user(0xdead) == None);
}

#[test]
fn task_can_spawn_a_better_priority_task_that_runs_after_it_gives_up_the_cpu() {
    let mut rig = Rig::new();
    let events = Rc::clone(&rig.events);
    rig.spawn("parent", 120, move |t| {
        events.borrow_mut().push("parent1");
        let child_events = Rc::clone(&events);
        t.spawn("child", prio(0), 0, move |_| child_events.borrow_mut().push("child")).unwrap();
        events.borrow_mut().push("parent2");
    });

    rig.poll();

    check!(rig.events() == ["parent1", "parent2", "child"]);
}

#[test]
fn raising_a_queued_tasks_priority_moves_it_ahead() {
    let mut rig = Rig::new();
    let a = rig.note("a");
    rig.spawn("a", 50, a);
    let b = rig.note("b");
    let b_id = rig.spawn("b", 60, b);
    let events = Rc::clone(&rig.events);
    rig.spawn("boss", 0, move |t| {
        t.set_priority(b_id, prio(10)).unwrap();
        events.borrow_mut().push("boss");
    });

    rig.poll();

    check!(rig.events() == ["boss", "b", "a"]);
}

#[test]
fn timer_task_wakes_when_the_clock_passes_the_deadline_and_the_deadline_is_rearmed() {
    let mut rig = Rig::new();
    let events = Rc::clone(&rig.events);
    let timer = rig.spawn("timer", 0, move |t| {
        t.arm_timer(Some(at(100)));
        t.schedule(true);
        events.borrow_mut().push("fired");
        t.arm_timer(Some(at(200)));
        t.schedule(true);
        events.borrow_mut().push("fired again");
    });
    rig.sched.set_timer_task(timer);

    let step = rig.poll();
    check!(step.next_deadline == Some(at(100)));
    check!(rig.fake.armed.borrow().last() == Some(&Some(at(100))));

    rig.fake.clock.set(at(99));
    rig.poll();
    check!(rig.events() == Vec::<&str>::new());

    rig.fake.clock.set(at(100));
    let step = rig.poll();
    check!(rig.events() == ["fired"]);
    check!(step.next_deadline == Some(at(200)));
    check!(rig.fake.armed.borrow().last() == Some(&Some(at(200))));

    rig.fake.clock.set(at(250));
    let step = rig.poll();
    check!(rig.events() == ["fired", "fired again"]);
    check!(step.next_deadline == None);
}

#[test]
fn irq_task_wakes_once_per_pending_irq() {
    let mut rig = Rig::new();
    let events = Rc::clone(&rig.events);
    let irq = rig.spawn("irq", 0, move |t| {
        loop {
            t.schedule(true);
            events.borrow_mut().push("irq");
        }
    });
    rig.sched.set_irq_task(irq);
    rig.poll();

    rig.fake.irq.set(true);
    rig.poll();
    rig.poll();

    check!(rig.events() == ["irq"]);
}

#[test]
fn budget_stops_the_step_between_tasks_and_reports_remaining_work() {
    let mut rig = Rig::new();
    for name in ["a", "b", "c"] {
        let events = Rc::clone(&rig.events);
        let clock = Rc::clone(&rig.fake.clock);
        rig.spawn(name, 120, move |_| {
            events.borrow_mut().push(name);
            clock.set(clock.get() + Duration::from_millis(10));
        });
    }

    let step = rig.sched.poll(Duration::from_millis(15)).unwrap();
    check!(rig.events() == ["a", "b"]);
    check!(step.ran == 2);
    check!(step.runnable);
    check!(rig.probe.quiescent.get() == 0);

    let step = rig.poll();
    check!(rig.events() == ["a", "b", "c"]);
    check!(!step.runnable);
}

#[test]
fn zero_budget_still_runs_one_task() {
    let mut rig = Rig::new();
    for name in ["a", "b"] {
        let body = rig.note(name);
        rig.spawn(name, 120, body);
    }
    let step = rig.sched.poll(Duration::ZERO).unwrap();
    check!(step.ran == 1);
    check!(step.runnable);
}

#[test]
fn quiescent_hook_runs_once_per_step_that_ends_parked() {
    let mut rig = Rig::new();
    for name in ["a", "b"] {
        rig.spawn(name, 120, |t| t.schedule(true));
    }
    rig.poll();
    check!(rig.probe.quiescent.get() == 1);
    rig.poll();
    check!(rig.probe.quiescent.get() == 2);
}

#[test]
fn on_enter_sees_the_time_of_each_schedule() {
    let mut rig = Rig::new();
    let clock = Rc::clone(&rig.fake.clock);
    rig.spawn("t", 120, move |t| {
        clock.set(at(10));
        t.schedule(false);
        clock.set(at(20));
        t.schedule(false);
    });
    rig.poll();
    check!(*rig.probe.entered.borrow() == [at(10), at(20)]);
}

#[test]
fn failed_check_surfaces_in_poll_and_stops_the_scheduler() {
    let mut rig = Rig::new();
    rig.probe.fail.set(true);
    let id = rig.spawn("t", 120, |t| t.schedule(false));

    check!(rig.sched.poll(HOUR) == Err(SchedError::CheckFailed { task: id, reason: "preempt_count" }));
    check!(rig.sched.poll(HOUR) == Err(SchedError::Poisoned));
}

#[test]
fn clobbered_canary_is_a_stack_overflow_and_stops_the_scheduler() {
    let mut rig = Rig::new();
    let limits = Rc::clone(&rig.src.limits);
    let id = rig.spawn("t", 120, move |t| {
        let limit = limits.borrow()[0];
        // SAFETY: the lowest word of the test stack is writable; this plays the part of an overflow.
        unsafe { (limit as *mut u64).write(0) };
        t.schedule(false);
    });

    check!(rig.sched.poll(HOUR) == Err(SchedError::StackOverflow { task: id }));
    check!(rig.sched.poll(HOUR) == Err(SchedError::Poisoned));
}

#[test]
fn poll_from_inside_a_task_is_refused() {
    let mut rig = Rig::new();
    let shared = Rc::clone(&rig.sched.shared);
    let result = Rc::new(RefCell::new(None));
    let out = Rc::clone(&result);
    rig.spawn("t", 120, move |_| *out.borrow_mut() = Some(shared.poll(Duration::ZERO)));

    rig.poll();

    check!(*result.borrow() == Some(Err(SchedError::PollInTask)));
}

#[inline(never)]
fn deep_frame() {
    let mut buf = [0_u8; 16 * 1024];
    for (i, b) in buf.iter_mut().enumerate() {
        *b = i as u8;
    }
    black_box(&buf);
}

#[test]
fn stack_used_grows_with_a_deep_frame() {
    let mut rig = Rig::new();
    let shallow = rig.spawn("shallow", 120, |t| t.schedule(true));
    let deep = rig.spawn("deep", 120, |t| {
        deep_frame();
        t.schedule(true);
    });

    rig.poll();

    let (shallow, deep) = (rig.sched.stack_used(shallow).unwrap(), rig.sched.stack_used(deep).unwrap());
    check!(deep >= 16 * 1024);
    check!(shallow < deep / 2);
}

#[test]
fn dropping_the_scheduler_releases_every_stack_including_suspended_and_unstarted_tasks() {
    let mut rig = Rig::new();
    rig.spawn("parked", 120, |t| t.schedule(true));
    rig.poll();
    let body = rig.note("never runs");
    rig.spawn("unstarted", 120, body);
    check!(rig.src.allocated.get() == 2);

    let (src, events) = (rig.src.clone(), Rc::clone(&rig.events));
    drop(rig);

    check!(src.released.get() == 2);
    check!(events.borrow().is_empty());
}

#[test]
fn spawn_reports_a_stack_source_that_cannot_allocate() {
    let mut rig = Rig::new();
    rig.src.fail.set(true);
    let result = rig.sched.spawn("t", prio(120), 0, |_| {});
    assert!(let Err(SpawnError::Stack { .. }) = result);
}
