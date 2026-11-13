//! A cooperative scheduler for the kernel threads the Linux C creates.
//!
//! Every task runs on its own stack (a corosensei coroutine) and gives the CPU up only by calling
//! [`Task::schedule`], so one core needs no locking. The app's main loop calls [`Scheduler::poll`] on
//! the main stack: it wakes the timer and IRQ tasks, runs tasks until all are parked or the budget is
//! spent, and says when to call again. The scheduler knows nothing about Linux or the platform; the
//! platform arrives through [`Platform`], [`StackSource`] and [`Hooks`].
//!
//! # Shared state
//!
//! Tasks reach the scheduler through the [`Task`] handle, while the main loop is suspended inside
//! `resume`. Code with no `Task` in hand (C calling into the shim) goes through a [`SchedHandle`], which
//! shares the state by reference and never through the `&mut Scheduler` the main loop holds, and reaches
//! the running task's own `Task` with [`SchedHandle::with_current`]. The state sits in `RefCell`s that
//! are borrowed only for short, non-reentrant sections and never across a `resume`, a `suspend`, a body
//! or a platform/hook callback. Execution is single threaded and cooperative, so a borrow is always released before anything else can run: a conflict
//! is a bug and panics instead of waiting, which means the cell can never deadlock.
//!
//! # Teardown
//!
//! Without corosensei's `unwind` feature a suspended coroutine cannot be dropped. Dropping the
//! scheduler therefore abandons suspended tasks with `force_reset`: their stack frames are never
//! unwound, so whatever they own leaks and no destructor runs. That is memory-safe because nothing
//! can resume them afterwards and the stack memory goes back to the [`StackSource`]; it relies on the
//! scheduler not being dropped while a task runs, which `poll` taking `&mut self` guarantees.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::fmt;
use core::ptr::NonNull;
use core::time::Duration;

use corosensei::stack::Stack;
use corosensei::{Coroutine, CoroutineResult, Yielder};
use ferrite_hal::Instant;
use snafu::{ResultExt, Snafu};

/// The stack size a task gets from the [`StackSource`] unless told otherwise.
pub const DEFAULT_STACK_SIZE: usize = 128 * 1024;

/// What the untouched part of a task stack is filled with, so the high-water mark can be measured.
const PAINT: u64 = 0x5afe_57ac_5afe_57ac;
/// What the lowest words of a task stack hold; a change means the stack ran past its end.
const CANARY: u64 = 0xca4a_17ca_ca4a_17ca;
const CANARY_WORDS: usize = 8;
const WORD: usize = size_of::<u64>();

/// Names one task. A slot is reused after its task exits, but never under the same id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TaskId {
    index: u32,
    generation: u32,
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "task {}.{}", self.index, self.generation)
    }
}

/// A Linux scheduling priority, 0 (runs first) to 139.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Priority(u8);

#[derive(Debug, Snafu)]
#[snafu(display("priority {level} is outside 0..={}", Priority::LOWEST.0))]
pub struct PriorityError {
    pub level: u8,
}

impl Priority {
    pub const HIGHEST: Self = Self(0);
    pub const LOWEST: Self = Self(139);

    pub const fn new(level: u8) -> Result<Self, PriorityError> {
        if level > Self::LOWEST.0 {
            return Err(PriorityError { level });
        }
        Ok(Self(level))
    }

    pub const fn get(self) -> u8 {
        self.0
    }
}

const LEVELS: usize = Priority::LOWEST.0 as usize + 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskState {
    Runnable,
    Blocked,
    /// What a finished task's id reports, since its slot is gone.
    Exited,
}

/// What `unblock` found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unblocked {
    /// The task was blocked and is runnable now.
    Woken,
    AlreadyRunnable,
    /// The task has exited, or the id never named one.
    Exited,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskInfo {
    pub name: String,
    pub priority: Priority,
    pub state: TaskState,
    pub user: usize,
}

/// What the scheduler needs from the machine it runs on.
pub trait Platform {
    fn now(&self) -> Instant;
    /// Make the platform wake the main loop at `at`, or cancel the wakeup.
    fn arm_wakeup(&mut self, at: Option<Instant>);
    /// Whether an interrupt arrived since the last call, clearing the flag.
    fn take_pending_irqs(&mut self) -> bool;
}

#[derive(Debug, Snafu)]
#[snafu(display("allocating a {size}-byte task stack failed"))]
pub struct StackAllocError {
    pub size: usize,
}

/// Where task stacks come from. The source decides how (pages, a pool, guard pages).
pub trait StackSource: 'static {
    type Stack: Stack + 'static;

    /// A stack of `size` bytes, aligned for the platform and owned by the caller until `release`.
    fn allocate(&mut self, size: usize) -> Result<Self::Stack, StackAllocError>;

    /// Take back a stack of an exited or abandoned task.
    fn release(&mut self, stack: Self::Stack);
}

/// Calls the C side makes from the scheduler's seams. Every method defaults to doing nothing.
pub trait Hooks: 'static {
    /// A task entered `schedule()`; time may have moved since the last entry.
    fn on_enter(&mut self, _now: Instant) {}

    /// Invariants that must hold at every `schedule()`; a failure is fatal to the scheduler.
    fn check(&self) -> Result<(), &'static str> {
        Ok(())
    }

    /// Every task is parked; called once per poll step that ends so. Runs on the main stack.
    fn quiescent(&mut self) {}
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NoHooks;

impl Hooks for NoHooks {}

#[derive(Debug, Snafu)]
pub enum SpawnError {
    #[snafu(display("no stack for the new task"))]
    Stack { source: StackAllocError },
}

#[derive(Debug, Snafu)]
#[snafu(display("{task} has exited"))]
pub struct TaskGone {
    pub task: TaskId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Snafu)]
pub enum SchedError {
    #[snafu(display("poll called from inside a task; it runs on the main stack only"))]
    PollInTask,
    #[snafu(display("{task} ran past the end of its stack"))]
    StackOverflow { task: TaskId },
    #[snafu(display("invariant check failed in {task}: {reason}"))]
    CheckFailed { task: TaskId, reason: &'static str },
    #[snafu(display("the scheduler stopped after an earlier fatal error"))]
    Poisoned,
}

type Body = Box<dyn FnOnce(&Task<'_>)>;
type Coro<St> = Coroutine<(), (), (), St>;

/// The operations a running task can reach, with the platform generics erased.
trait Core {
    fn now(&self) -> Instant;
    /// Record the yielder of a task whose body just started.
    fn started(&self, id: TaskId, yielder: NonNull<Yielder<(), ()>>);
    /// The running task and its yielder; `None` on the main stack.
    fn running(&self) -> Option<(TaskId, NonNull<Yielder<(), ()>>)>;
    fn user(&self, id: TaskId) -> Option<usize>;
    fn find_by_user(&self, user: usize) -> Option<TaskId>;
    /// Mark the running task to be torn down once it next suspends.
    fn exit(&self, id: TaskId);
    fn enter(&self, id: TaskId, block: bool);
    fn unblock(&self, id: TaskId) -> Unblocked;
    fn spawn(&self, name: String, priority: Priority, user: usize, body: Body) -> Result<TaskId, SpawnError>;
    fn set_priority(&self, id: TaskId, priority: Priority) -> Result<(), TaskGone>;
    fn arm_timer(&self, at: Option<Instant>);
}

/// A running task's view of the scheduler, passed to its body.
pub struct Task<'a> {
    core: &'a dyn Core,
    yielder: &'a Yielder<(), ()>,
    id: TaskId,
}

impl Task<'_> {
    /// Give up the CPU. With `block` the task stays parked until someone unblocks it; without, it
    /// goes to the tail of its priority's queue and stays runnable.
    pub fn schedule(&self, block: bool) {
        self.core.enter(self.id, block);
        self.yielder.suspend(());
    }

    pub const fn current(&self) -> TaskId {
        self.id
    }

    /// The `user` word this task was spawned with.
    pub fn user(&self) -> usize {
        self.core.user(self.id).unwrap_or_default()
    }

    /// End this task from anywhere in its call stack. Its frames are abandoned, never unwound, so
    /// whatever they own leaks; the stack goes back to the [`StackSource`].
    pub fn exit(&self) -> ! {
        self.core.exit(self.id);
        self.yielder.suspend(());
        unreachable!("an exited task was resumed")
    }

    pub fn unblock(&self, id: TaskId) -> Unblocked {
        self.core.unblock(id)
    }

    pub fn spawn(
        &self,
        name: impl Into<String>,
        priority: Priority,
        user: usize,
        body: impl FnOnce(&Task<'_>) + 'static,
    ) -> Result<TaskId, SpawnError> {
        self.core.spawn(name.into(), priority, user, Box::new(body))
    }

    pub fn set_priority(&self, id: TaskId, priority: Priority) -> Result<(), TaskGone> {
        self.core.set_priority(id, priority)
    }

    /// Set the one timer deadline, or cancel it. The timer task is woken once `now` passes it.
    pub fn arm_timer(&self, at: Option<Instant>) {
        self.core.arm_timer(at);
    }

    pub fn now(&self) -> Instant {
        self.core.now()
    }
}

/// The address range of a task stack, `limit` lowest.
#[derive(Clone, Copy)]
struct Region {
    limit: usize,
    base: usize,
}

impl Region {
    /// First whole word and the number of whole words in the range.
    fn span(self) -> (usize, usize) {
        let start = self.limit.next_multiple_of(WORD);
        let end = self.base & !(WORD - 1);
        (start, end.saturating_sub(start) / WORD)
    }

    fn word(start: usize, index: usize) -> *mut u64 {
        core::ptr::with_exposed_provenance_mut(start + index * WORD)
    }

    /// Fill the stack with `PAINT` and the lowest words with `CANARY`.
    ///
    /// # Safety
    /// The range must be writable memory that nothing else is using.
    unsafe fn paint(self) {
        let (start, words) = self.span();
        for i in 0..words {
            let value = if i < CANARY_WORDS { CANARY } else { PAINT };
            // SAFETY: `i` is inside the whole-word range the caller vouches for.
            unsafe { Self::word(start, i).write(value) };
        }
    }

    /// Whether the lowest words still hold the canary.
    ///
    /// # Safety
    /// The range must be readable memory that `paint` ran over.
    unsafe fn canary_intact(self) -> bool {
        let (start, words) = self.span();
        // SAFETY: `i` is inside the range the caller vouches for.
        (0..CANARY_WORDS.min(words)).all(|i| unsafe { Self::word(start, i).read() } == CANARY)
    }

    /// Bytes from the top of the stack to the deepest word that is not paint.
    ///
    /// # Safety
    /// The range must be readable memory that `paint` ran over.
    unsafe fn used(self) -> usize {
        let (start, words) = self.span();
        let untouched = (CANARY_WORDS..words)
            // SAFETY: `i` is inside the range the caller vouches for.
            .take_while(|&i| unsafe { Self::word(start, i).read() } == PAINT)
            .count();
        self.base - (start + (CANARY_WORDS.min(words) + untouched) * WORD)
    }
}

struct Entry<St: Stack> {
    name: String,
    priority: Priority,
    state: TaskState,
    user: usize,
    queued: bool,
    stack: Region,
    /// Set once the body starts; valid while the task is alive, because the coroutine owns it.
    yielder: Option<NonNull<Yielder<(), ()>>>,
    /// The task called `exit`; the poll that sees it suspend tears it down.
    exiting: bool,
    /// Taken out while the task runs, and while the slot is reserved but not yet filled.
    co: Option<Coro<St>>,
}

struct Slot<St: Stack> {
    generation: u32,
    entry: Option<Entry<St>>,
}

struct State<St: Stack> {
    slots: Vec<Slot<St>>,
    free: Vec<u32>,
    queues: Vec<VecDeque<TaskId>>,
    current: Option<TaskId>,
    deadline: Option<Instant>,
    timer_task: Option<TaskId>,
    irq_task: Option<TaskId>,
    /// An error a task's `schedule` found, reported by the poll that sees the task switch back.
    fault: Option<SchedError>,
    poisoned: bool,
}

impl<St: Stack> State<St> {
    fn entry_mut(&mut self, id: TaskId) -> Option<&mut Entry<St>> {
        let slot = self.slots.get_mut(id.index as usize)?;
        if slot.generation == id.generation { slot.entry.as_mut() } else { None }
    }

    fn entry(&self, id: TaskId) -> Option<&Entry<St>> {
        let slot = self.slots.get(id.index as usize)?;
        if slot.generation == id.generation { slot.entry.as_ref() } else { None }
    }

    fn enqueue(&mut self, id: TaskId) {
        let Some(entry) = self.entry_mut(id) else { return };
        entry.queued = true;
        let level = usize::from(entry.priority.0);
        self.queues[level].push_back(id);
    }

    fn wake(&mut self, id: TaskId) -> Unblocked {
        match self.entry_mut(id) {
            None => Unblocked::Exited,
            Some(entry) if entry.state == TaskState::Runnable => Unblocked::AlreadyRunnable,
            Some(entry) => {
                entry.state = TaskState::Runnable;
                self.enqueue(id);
                Unblocked::Woken
            },
        }
    }

    fn reserve(&mut self, name: String, priority: Priority, user: usize, stack: Region) -> TaskId {
        let entry = Entry {
            name,
            priority,
            state: TaskState::Runnable,
            user,
            queued: false,
            stack,
            yielder: None,
            exiting: false,
            co: None,
        };
        if let Some(index) = self.free.pop() {
            let slot = &mut self.slots[index as usize];
            slot.entry = Some(entry);
            TaskId { index, generation: slot.generation }
        } else {
            let index = u32::try_from(self.slots.len()).unwrap_or(u32::MAX);
            self.slots.push(Slot { generation: 0, entry: Some(entry) });
            TaskId { index, generation: 0 }
        }
    }

    fn release(&mut self, id: TaskId) {
        if let Some(slot) = self.slots.get_mut(id.index as usize)
            && slot.generation == id.generation
            && slot.entry.take().is_some()
        {
            slot.generation = slot.generation.wrapping_add(1);
            self.free.push(id.index);
        }
    }

    fn pick(&mut self) -> Option<TaskId> {
        let id = self.queues.iter_mut().find_map(VecDeque::pop_front)?;
        if let Some(entry) = self.entry_mut(id) {
            entry.queued = false;
        }
        Some(id)
    }
}

struct Shared<P, S: StackSource, H> {
    state: RefCell<State<S::Stack>>,
    platform: RefCell<P>,
    stacks: RefCell<S>,
    hooks: RefCell<H>,
}

impl<P: Platform + 'static, S: StackSource, H: Hooks> Shared<P, S, H> {
    fn spawn_task(&self, name: String, priority: Priority, user: usize, body: Body) -> Result<TaskId, SpawnError> {
        let size = DEFAULT_STACK_SIZE;
        let stack = self.stacks.borrow_mut().allocate(size).context(StackSnafu)?;
        let region = Region { limit: stack.limit().get(), base: stack.base().get() };
        // SAFETY: the `Stack` contract makes the range owned, writable memory, and no coroutine runs on it yet.
        unsafe { region.paint() };

        let id = self.state.borrow_mut().reserve(name, priority, user, region);
        let core: *const dyn Core = self;
        let co = Coroutine::with_stack(stack, move |yielder: &Yielder<(), ()>, ()| {
            // SAFETY: the scheduler owns the `Shared` this points into and never frees it while a
            // task can still run: `Drop for Scheduler` abandons every coroutine first.
            let core = unsafe { &*core };
            core.started(id, NonNull::from(yielder));
            body(&Task { core, yielder, id });
        });
        let mut st = self.state.borrow_mut();
        if let Some(entry) = st.entry_mut(id) {
            entry.co = Some(co);
        }
        st.enqueue(id);
        drop(st);
        log::debug!("spawned {id} at priority {}, stack {size} bytes", priority.0);
        Ok(id)
    }

    /// Run `id` until it suspends or returns, then check what it left behind.
    fn run(&self, id: TaskId) -> Result<(), SchedError> {
        let taken = {
            let mut st = self.state.borrow_mut();
            st.current = Some(id);
            st.entry_mut(id).and_then(|e| e.co.take().map(|co| (co, e.stack)))
        };
        let Some((mut co, region)) = taken else {
            self.state.borrow_mut().current = None;
            log::error!("{id} was queued without a coroutine");
            return Ok(());
        };

        let result = co.resume(());

        // SAFETY: the range was painted at spawn and the task is suspended or finished.
        let intact = unsafe { region.canary_intact() };
        let mut st = self.state.borrow_mut();
        st.current = None;
        if !intact {
            st.poisoned = true;
            if let Some(entry) = st.entry_mut(id) {
                entry.co = Some(co);
            }
            log::error!("{id} overran its stack; the scheduler stops");
            return Err(SchedError::StackOverflow { task: id });
        }
        let fault = st.fault.take();
        match result {
            CoroutineResult::Yield(()) if st.entry(id).is_some_and(|e| e.exiting) => {
                st.release(id);
                drop(st);
                log::debug!("{id} exited from inside its body");
                // SAFETY: the task is suspended for good and its slot is gone, so nothing can resume it;
                // its frames leak, as in "Teardown" in the module docs.
                unsafe { co.force_reset() };
                self.stacks.borrow_mut().release(co.into_stack());
                st = self.state.borrow_mut();
            },
            CoroutineResult::Yield(()) => {
                if let Some(entry) = st.entry_mut(id) {
                    entry.co = Some(co);
                }
            },
            CoroutineResult::Return(()) => {
                st.release(id);
                drop(st);
                log::debug!("{id} exited");
                self.stacks.borrow_mut().release(co.into_stack());
                st = self.state.borrow_mut();
            },
        }
        if let Some(fault) = fault {
            st.poisoned = true;
            return Err(fault);
        }
        Ok(())
    }

    fn poll(&self, budget: Duration) -> Result<Step, SchedError> {
        {
            let st = self.state.borrow();
            if st.current.is_some() {
                return Err(SchedError::PollInTask);
            }
            if st.poisoned {
                return Err(SchedError::Poisoned);
            }
        }

        let now = self.platform.borrow().now();
        let irqs = self.platform.borrow_mut().take_pending_irqs();
        {
            let mut st = self.state.borrow_mut();
            if st.deadline.is_some_and(|d| d <= now) {
                st.deadline = None;
                if let Some(timer) = st.timer_task {
                    st.wake(timer);
                }
            }
            if irqs {
                match st.irq_task {
                    Some(irq) => {
                        st.wake(irq);
                    },
                    None => log::debug!("interrupts pending but no irq task is set"),
                }
            }
        }

        let end = now.checked_add(budget);
        let mut ran = 0;
        let drained = loop {
            // A task always gets to run once, so a zero budget still makes progress.
            if ran > 0 && end.is_some_and(|end| self.platform.borrow().now() >= end) {
                break false;
            }
            let Some(id) = self.state.borrow_mut().pick() else { break true };
            self.run(id)?;
            ran += 1;
        };
        if drained {
            self.hooks.borrow_mut().quiescent();
        }

        let (runnable, next_deadline) = {
            let st = self.state.borrow();
            (st.queues.iter().any(|q| !q.is_empty()), st.deadline)
        };
        self.platform.borrow_mut().arm_wakeup(next_deadline);
        Ok(Step { runnable, next_deadline, ran })
    }
}

impl<P: Platform + 'static, S: StackSource, H: Hooks> Core for Shared<P, S, H> {
    fn now(&self) -> Instant {
        self.platform.borrow().now()
    }

    fn started(&self, id: TaskId, yielder: NonNull<Yielder<(), ()>>) {
        if let Some(entry) = self.state.borrow_mut().entry_mut(id) {
            entry.yielder = Some(yielder);
        }
    }

    fn running(&self) -> Option<(TaskId, NonNull<Yielder<(), ()>>)> {
        let st = self.state.borrow();
        let id = st.current?;
        Some((id, st.entry(id)?.yielder?))
    }

    fn user(&self, id: TaskId) -> Option<usize> {
        self.state.borrow().entry(id).map(|e| e.user)
    }

    fn find_by_user(&self, user: usize) -> Option<TaskId> {
        let st = self.state.borrow();
        st.slots.iter().enumerate().find_map(|(index, slot)| {
            let entry = slot.entry.as_ref()?;
            (entry.user == user)
                .then(|| TaskId { index: u32::try_from(index).unwrap_or(u32::MAX), generation: slot.generation })
        })
    }

    fn exit(&self, id: TaskId) {
        if let Some(entry) = self.state.borrow_mut().entry_mut(id) {
            entry.exiting = true;
            entry.state = TaskState::Blocked;
        }
    }

    fn enter(&self, id: TaskId, block: bool) {
        let now = self.now();
        self.hooks.borrow_mut().on_enter(now);
        let checked = self.hooks.borrow().check();
        let mut st = self.state.borrow_mut();
        if let Err(reason) = checked {
            log::error!("invariant check failed entering schedule in {id}: {reason}");
            st.fault.get_or_insert(SchedError::CheckFailed { task: id, reason });
        }
        if let Some(entry) = st.entry_mut(id) {
            if block {
                entry.state = TaskState::Blocked;
            } else {
                entry.state = TaskState::Runnable;
                st.enqueue(id);
            }
        }
    }

    fn unblock(&self, id: TaskId) -> Unblocked {
        self.state.borrow_mut().wake(id)
    }

    fn spawn(&self, name: String, priority: Priority, user: usize, body: Body) -> Result<TaskId, SpawnError> {
        self.spawn_task(name, priority, user, body)
    }

    fn set_priority(&self, id: TaskId, priority: Priority) -> Result<(), TaskGone> {
        let mut st = self.state.borrow_mut();
        let entry = st.entry_mut(id).ok_or(TaskGone { task: id })?;
        let old = usize::from(entry.priority.0);
        entry.priority = priority;
        if entry.queued {
            st.queues[old].retain(|&queued| queued != id);
            st.enqueue(id);
        }
        Ok(())
    }

    fn arm_timer(&self, at: Option<Instant>) {
        self.state.borrow_mut().deadline = at;
    }
}

/// What one `poll` did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Step {
    /// Runnable tasks remain, so the caller should poll again without waiting.
    pub runnable: bool,
    /// When the timer task is next due, also handed to `Platform::arm_wakeup`.
    pub next_deadline: Option<Instant>,
    /// How many task runs the step made.
    pub ran: usize,
}

/// A shared view of a [`Scheduler`], for callers that cannot be handed a [`Task`].
///
/// C calling the shim is one. The handle works through `&self` only, so a call from inside a running
/// task never aliases the `&mut Scheduler` of the `poll` that is running it.
#[derive(Clone)]
pub struct SchedHandle {
    core: Rc<dyn Core>,
}

impl SchedHandle {
    /// Run `f` with the running task's own [`Task`]; `None` on the main stack.
    pub fn with_current<R>(&self, f: impl FnOnce(&Task<'_>) -> R) -> Option<R> {
        let (id, yielder) = self.core.running()?;
        // SAFETY: the yielder belongs to the running task's coroutine, which is alive while it runs,
        // and this call is on that task's stack, so the reference cannot outlive it.
        let yielder = unsafe { yielder.as_ref() };
        Some(f(&Task { core: &*self.core, yielder, id }))
    }

    pub fn spawn(
        &self,
        name: impl Into<String>,
        priority: Priority,
        user: usize,
        body: impl FnOnce(&Task<'_>) + 'static,
    ) -> Result<TaskId, SpawnError> {
        self.core.spawn(name.into(), priority, user, Box::new(body))
    }

    pub fn unblock(&self, id: TaskId) -> Unblocked {
        self.core.unblock(id)
    }

    pub fn set_priority(&self, id: TaskId, priority: Priority) -> Result<(), TaskGone> {
        self.core.set_priority(id, priority)
    }

    /// The first live task spawned with this `user` word.
    pub fn find_by_user(&self, user: usize) -> Option<TaskId> {
        self.core.find_by_user(user)
    }
}

/// The scheduler. It holds no globals; see the module docs for how tasks reach it.
pub struct Scheduler<P: Platform + 'static, S: StackSource, H: Hooks = NoHooks> {
    shared: Rc<Shared<P, S, H>>,
}

impl<P: Platform + 'static, S: StackSource, H: Hooks> Scheduler<P, S, H> {
    pub fn new(platform: P, stacks: S, hooks: H) -> Self {
        let queues = (0..LEVELS).map(|_| VecDeque::new()).collect();
        let state = State {
            slots: Vec::new(),
            free: Vec::new(),
            queues,
            current: None,
            deadline: None,
            timer_task: None,
            irq_task: None,
            fault: None,
            poisoned: false,
        };
        Self {
            shared: Rc::new(Shared {
                state: RefCell::new(state),
                platform: RefCell::new(platform),
                stacks: RefCell::new(stacks),
                hooks: RefCell::new(hooks),
            }),
        }
    }

    /// Create a task that runs `body` on its own stack, and queue it. `user` is the C side's word.
    pub fn spawn(
        &mut self,
        name: impl Into<String>,
        priority: Priority,
        user: usize,
        body: impl FnOnce(&Task<'_>) + 'static,
    ) -> Result<TaskId, SpawnError> {
        self.shared.spawn_task(name.into(), priority, user, Box::new(body))
    }

    /// The task woken when the armed deadline passes.
    pub fn set_timer_task(&mut self, id: TaskId) {
        self.shared.state.borrow_mut().timer_task = Some(id);
    }

    /// The task woken when the platform reports pending interrupts.
    pub fn set_irq_task(&mut self, id: TaskId) {
        self.shared.state.borrow_mut().irq_task = Some(id);
    }

    pub fn unblock(&mut self, id: TaskId) -> Unblocked {
        self.shared.unblock(id)
    }

    pub fn info(&self, id: TaskId) -> Option<TaskInfo> {
        let st = self.shared.state.borrow();
        st.entry(id).map(|e| TaskInfo { name: e.name.clone(), priority: e.priority, state: e.state, user: e.user })
    }

    /// The first live task spawned with this `user` word.
    pub fn find_by_user(&self, user: usize) -> Option<TaskId> {
        self.shared.find_by_user(user)
    }

    /// A handle for code that runs inside tasks and has no [`Task`] passed to it.
    pub fn handle(&self) -> SchedHandle {
        SchedHandle { core: self.shared.clone() }
    }

    /// Bytes of the task's stack that have been touched, or `None` if it has exited.
    pub fn stack_used(&self, id: TaskId) -> Option<usize> {
        let region = self.shared.state.borrow().entry(id)?.stack;
        // SAFETY: the range was painted at spawn and stays allocated while the task is alive.
        Some(unsafe { region.used() })
    }

    /// One step of the main loop; see the module docs.
    pub fn poll(&mut self, budget: Duration) -> Result<Step, SchedError> {
        self.shared.poll(budget)
    }
}

impl<P: Platform + 'static, S: StackSource, H: Hooks> Drop for Scheduler<P, S, H> {
    fn drop(&mut self) {
        let coroutines: Vec<_> = {
            let mut st = self.shared.state.borrow_mut();
            st.slots.iter_mut().filter_map(|slot| slot.entry.as_mut()?.co.take()).collect()
        };
        for mut co in coroutines {
            if co.started() && !co.done() {
                // SAFETY: see "Teardown" in the module docs; the frames are leaked, never resumed.
                unsafe { co.force_reset() };
            } else {
                // Unstarted: drops the body. Finished: nothing to do.
                co.force_unwind();
            }
            self.shared.stacks.borrow_mut().release(co.into_stack());
        }
    }
}

#[cfg(test)]
mod tests;
