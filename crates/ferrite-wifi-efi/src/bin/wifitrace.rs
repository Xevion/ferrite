//! `wifitrace`: the Linux Wi-Fi C under UEFI, run until it reaches a kernel symbol nothing implements.
//!
//! The initcalls run in table order on a 128 KiB task stack switched to with corosensei, while a 1 ms
//! firmware timer event counts ticks, so firmware interrupts land on the task stack as they will under
//! the scheduler. The first stub switches back to the main stack for good; the app reports the stub,
//! the ticks seen and how much of the task stack was touched, then exits to the firmware or the loader.
//!
//! It is the UEFI counterpart of `ferrite-lab`, and proves the image that links the ELF C boots.

#![no_std]
#![no_main]

use core::ffi::{CStr, c_void};
use core::num::NonZeroUsize;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use core::time::Duration;

use corosensei::stack::{Stack, StackPointer};
use corosensei::{Coroutine, CoroutineResult, Yielder};
use ferrite_efi::bootlog::{BootLog, Echo};
use ferrite_efi::clock::Clock;
use ferrite_efi::{logger, session};
use ferrite_hal::Delay;
use log::LevelFilter;
use uefi::boot::{self, AllocateType, EventType, MemoryType, TimerTrigger, Tpl};
use uefi::prelude::*;

const PAGE: usize = 4096;
const STACK_SIZE: usize = 128 * 1024;
/// What the task stack is filled with first, so the untouched part can be measured afterwards.
const PAINT: u64 = 0x5afe_57ac_5afe_57ac;
const TICK: Duration = Duration::from_millis(1);
/// How long each initcall's task busy-waits first, so timer interrupts arrive on the task stack.
const SETTLE: Duration = Duration::from_millis(50);
const EXIT_AFTER: Duration = Duration::from_secs(10);

static TICKS: AtomicU64 = AtomicU64::new(0);
/// The running task's yielder, for the halt hook, which runs on the task stack.
static YIELDER: AtomicPtr<Yielder<(), Step>> = AtomicPtr::new(core::ptr::null_mut());

/// Why the task gave the main stack control.
enum Step {
    /// A deliberate round trip, to exercise the switch.
    Switch,
    /// A stub was hit; the task is never resumed.
    Halted(&'static CStr),
}

/// Pages from the firmware, owned for the life of the app and never freed, so copies are plain handles.
#[derive(Clone, Copy)]
struct TaskStack {
    low: NonNull<u64>,
}

impl TaskStack {
    fn allocate() -> uefi::Result<Self> {
        let pages = boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, STACK_SIZE / PAGE)?;
        let low = pages.cast::<u64>();
        // SAFETY: the firmware just handed over these pages, page aligned and unaliased.
        unsafe { core::slice::from_raw_parts_mut(low.as_ptr(), STACK_SIZE / 8) }.fill(PAINT);
        Ok(Self { low })
    }

    /// Bytes from the top that something has written since the paint.
    fn used(&self) -> usize {
        // SAFETY: the pages stay allocated, and the task that wrote them is suspended for good.
        let words = unsafe { core::slice::from_raw_parts(self.low.as_ptr(), STACK_SIZE / 8) };
        STACK_SIZE - words.iter().take_while(|&&w| w == PAINT).count() * 8
    }

    fn low(&self) -> usize {
        self.low.as_ptr() as usize
    }
}

// SAFETY: the range is page aligned, at least `MIN_STACK_SIZE`, and owned by the app; it has no
// guard page yet, which a tracer that stops at the first stub accepts.
unsafe impl Stack for TaskStack {
    fn base(&self) -> StackPointer {
        NonZeroUsize::new(self.low() + STACK_SIZE).unwrap_or(NonZeroUsize::MIN)
    }

    fn limit(&self) -> StackPointer {
        NonZeroUsize::new(self.low()).unwrap_or(NonZeroUsize::MIN)
    }
}

unsafe extern "efiapi" fn tick(_: uefi::Event, _: Option<NonNull<c_void>>) {
    TICKS.fetch_add(1, Ordering::Relaxed);
}

/// The halt hook: switch back to the main stack and never return to the C.
fn halt(stub: &CStr) -> ! {
    let yielder = YIELDER.load(Ordering::Acquire);
    if !yielder.is_null() {
        // SAFETY: the stub names are string literals in the image, so they live as long as it does.
        let stub = unsafe { &*core::ptr::from_ref(stub) };
        // SAFETY: the pointer was stored by the running task, and its yielder outlives its suspends.
        unsafe { &*yielder }.suspend(Step::Halted(stub));
    }
    loop {
        core::hint::spin_loop();
    }
}

#[entry]
fn main() -> Status {
    let helpers = uefi::helpers::init();
    logger::init(BootLog::open("wifitrace", Echo::Console), LevelFilter::Info);
    if let Err(e) = helpers {
        log::warn!("uefi helpers init failed: {e}");
    }
    let status = run();
    logger::with(|log| session::finish(log, EXIT_AFTER));
    status
}

fn run() -> Status {
    if let Err(e) = ferrite_lkpi::set_halt_hook(halt) {
        log::error!("{e}");
        return Status::ABORTED;
    }
    let stack = match TaskStack::allocate() {
        Ok(stack) => stack,
        Err(e) => {
            log::error!("allocating the {STACK_SIZE}-byte task stack failed: {e}");
            return Status::OUT_OF_RESOURCES;
        },
    };
    // SAFETY: the notify function only bumps an atomic.
    let timer =
        unsafe { boot::create_event(EventType::TIMER | EventType::NOTIFY_SIGNAL, Tpl::CALLBACK, Some(tick), None) }
            .and_then(|event| boot::set_timer(&event, TimerTrigger::Periodic(TICK)).map(|()| event));
    let timer = match timer {
        Ok(timer) => timer,
        Err(e) => {
            log::error!("starting the {TICK:?} timer event failed: {e}");
            return Status::ABORTED;
        },
    };

    let status = trace(stack);
    if let Err(e) = boot::close_event(timer) {
        log::debug!("closing the timer event failed: {e}");
    }
    status
}

fn trace(stack: TaskStack) -> Status {
    let clock = Clock::start();
    let calls = ferrite_wifi_sys::initcalls();
    log::info!("{} initcalls; task stack {STACK_SIZE} bytes at {:#x}", calls.len(), stack.low());

    let mut task = Coroutine::with_stack(stack, move |yielder: &Yielder<(), Step>, ()| {
        YIELDER.store(core::ptr::from_ref(yielder).cast_mut(), Ordering::Release);
        for call in calls {
            let name = call.name().to_string_lossy();
            log::info!("initcall level {} {name}", call.level().to_string_lossy());
            clock.delay(SETTLE);
            yielder.suspend(Step::Switch);
            // SAFETY: this task is the only caller, runs the calls one at a time, and the image links the shim.
            let code = unsafe { call.call() };
            log::info!("initcall {name} returned {code}");
        }
    });

    let mut switches = 0_u32;
    loop {
        match task.resume(()) {
            CoroutineResult::Yield(Step::Switch) => switches += 1,
            CoroutineResult::Yield(Step::Halted(stub)) => {
                log::info!(
                    "halted in stub {} after {switches} switches and {} timer ticks; task stack used {} of {STACK_SIZE} bytes",
                    stub.to_string_lossy(),
                    TICKS.load(Ordering::Relaxed),
                    stack.used(),
                );
                // Dropping a suspended coroutine needs unwinding, which this build has not.
                core::mem::forget(task);
                return Status::SUCCESS;
            },
            CoroutineResult::Return(()) => {
                log::info!("every initcall returned, after {switches} switches");
                return Status::SUCCESS;
            },
        }
    }
}
