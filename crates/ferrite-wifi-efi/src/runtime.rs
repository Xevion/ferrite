//! The Linux shim under UEFI, brought up and kept running for an app.
//!
//! Start-up installs the halt hook, the kernel's random seed from firmware entropy, the PCI cards, the
//! kernel arena (after the DMA protection decision), the firmware file provider and the wall clock, starts
//! a 1 ms firmware timer that counts ticks and reports a stalled main loop, and creates the scheduler
//! with kthreadd. The app then starts one task on it (the test script of `ferrite-wifi-start`, or the
//! station service) and calls [`Runtime::poll`] from its own main loop.
//!
//! No Linux code runs outside a scheduler task, and tasks run only inside `poll`, on the caller's stack.
//! The timer notify only bumps a counter. A `Runtime` lives once per program: every piece it installs
//! into the shim can be installed only once.

use alloc::boxed::Box;
use core::ffi::{CStr, c_char, c_void};
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use core::time::Duration;

use ferrite_efi::bootlog::BootLog;
use ferrite_efi::clock::Clock;
use ferrite_efi::entropy::FirmwareRngError;
use ferrite_efi::{entropy, logger, rtc};
use ferrite_hal::{Clock as _, Delay as _, Instant};
use ferrite_lkpi::HookAlreadySet;
use ferrite_lkpi::firmware::{FirmwareProvider, ProviderAlreadyInstalled};
use ferrite_lkpi::mem::ArenaAlreadyInstalled;
use ferrite_lkpi::pci::PciAlreadyInstalled;
use ferrite_lkpi::random::AlreadySeeded;
use ferrite_lkpi::sched::{DEFAULT_STACK_SIZE, NoHooks, SchedError, SchedHandle, Scheduler, Step, TaskState};
use ferrite_lkpi::task::AlreadyInstalled;
use ferrite_lkpi::time::WallClockAlreadySet;
use snafu::Snafu;
use uefi::boot::{self, EventType, OpenProtocolAttributes, OpenProtocolParams, TimerTrigger, Tpl};
use uefi::proto::loaded_image::LoadedImage;
use uefi::{Event, Status};

use crate::arena::{self, ArenaPages};
use crate::firmware::VolumeFirmware;
use crate::iommu::{self, DmaPath};
use crate::pci::UefiPci;
use crate::platform::{IRQ_POLL, UefiPlatform};
use crate::stacks::{PageStacks, StackStats};

/// Kernel memory, from the first size the firmware has a 4 MiB-aligned range below 4 GiB for. A
/// station uses a small fraction of 64 MiB; rtw89 and iwlwifi buffers fit in it with room.
pub const ARENA_SIZES: [usize; 3] = [64 << 20, 32 << 20, 16 << 20];

/// Exit status of a run that stopped in a stub: an OEM error code, distinct from every standard one.
pub const EXIT_HALTED: Status = Status(Status::ERROR_BIT | 0x4000_0000_0000_0000 | 0x56);

const TICK: Duration = Duration::from_millis(1);
/// How far above the timer notify's own frame the interrupted stack is scanned, and how many code
/// addresses found there are reported.
const STALL_SCAN: usize = 32 << 10;
const STALL_FRAMES: usize = 32;

static TICKS: AtomicU64 = AtomicU64::new(0);
/// The tick of the main loop's last poll; 0 while the loop is deliberately asleep or not polling yet.
static PROGRESS: AtomicU64 = AtomicU64::new(0);
/// Ticks without a poll after which a task is taken to be spinning; `u64::MAX` for never.
static STALL_TICKS: AtomicU64 = AtomicU64::new(u64::MAX);
static STALL_REPORTED: AtomicBool = AtomicBool::new(false);
/// This image's load address and size, for telling code addresses on a stack apart.
static IMAGE_BASE: AtomicUsize = AtomicUsize::new(0);
static IMAGE_SIZE: AtomicUsize = AtomicUsize::new(0);
/// The stub that stopped the run, null until one does.
static HALTED: AtomicPtr<c_char> = AtomicPtr::new(core::ptr::null_mut());

/// What the runtime is asked to do at start-up.
#[derive(Clone, Copy, Debug)]
pub struct RuntimeConfig {
    /// Arena sizes to try in order; each a multiple of the page size.
    pub arena_sizes: &'static [usize],
    /// How long the main loop may go without polling before a spinning task is assumed: the stall is
    /// logged and the cards are taken off the bus. `None` for a main loop that may be busy for longer.
    pub stall_limit: Option<Duration>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self { arena_sizes: &ARENA_SIZES, stall_limit: Some(Duration::from_secs(3)) }
    }
}

#[derive(Debug, Snafu)]
pub enum RuntimeError {
    #[snafu(display("{source}"))]
    HaltHook { source: HookAlreadySet },
    #[snafu(display("refusing to start without firmware entropy: {source}"))]
    Entropy { source: FirmwareRngError },
    #[snafu(display("{source}"))]
    Seed { source: AlreadySeeded },
    #[snafu(display("no kernel arena could be allocated"))]
    NoArena,
    #[snafu(display("{source}"))]
    ArenaInstall { source: ArenaAlreadyInstalled },
    #[snafu(display("{source}"))]
    PciInstall { source: PciAlreadyInstalled },
    #[snafu(display("{source}"))]
    FirmwareInstall { source: ProviderAlreadyInstalled },
    #[snafu(display("{source}"))]
    WallClock { source: WallClockAlreadySet },
    #[snafu(display("starting the {TICK:?} timer event failed: {source}"))]
    Timer { source: uefi::Error },
    #[snafu(display("{source}"))]
    TaskInstall { source: AlreadyInstalled },
    #[snafu(display("starting kthreadd failed: {code}"))]
    Kthreadd { code: core::ffi::c_int },
    #[snafu(display("{source}"))]
    Sched { source: SchedError },
}

impl RuntimeError {
    /// The status an app that cannot go on without Wi-Fi exits with.
    pub const fn status(&self) -> Status {
        match self {
            Self::Entropy { .. } => Status::UNSUPPORTED,
            Self::NoArena => Status::OUT_OF_RESOURCES,
            _ => Status::ABORTED,
        }
    }
}

/// How soon the app must poll again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NextPoll {
    /// Tasks are still runnable: poll again without waiting.
    Now,
    /// The Linux side wants the CPU again after this long (a timer is due, or devices need their
    /// interrupt handlers run).
    After(Duration),
    /// Every task is blocked and nothing will wake one: poll again when the app has a reason to.
    Idle,
}

unsafe extern "efiapi" fn tick(_: Event, _: Option<NonNull<c_void>>) {
    let now = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    let last = PROGRESS.load(Ordering::Relaxed);
    if last != 0 && now - last > STALL_TICKS.load(Ordering::Relaxed) && !STALL_REPORTED.swap(true, Ordering::Relaxed) {
        report_stall(now - last);
    }
}

/// A task has kept the CPU past the stall limit without yielding, so the run limit can never fire. From
/// the timer notify that interrupted it: log where it is, as code addresses found on the interrupted
/// stack (offsets into the image, the first ones nearest the spin), and take bus mastering off the
/// cards. The loader's watchdog ends the run.
fn report_stall(ticks: u64) {
    let (base, size) = (IMAGE_BASE.load(Ordering::Relaxed), IMAGE_SIZE.load(Ordering::Relaxed));
    let rsp: usize;
    // SAFETY: reads the stack pointer into a register; no memory or flags are touched.
    unsafe { core::arch::asm!("mov {}, rsp", out(reg) rsp, options(nomem, nostack, preserves_flags)) };
    let mut frames = alloc::vec::Vec::with_capacity(STALL_FRAMES);
    let mut at = rsp;
    while at < rsp + STALL_SCAN && frames.len() < STALL_FRAMES {
        // SAFETY: firmware memory is identity mapped and readable; the scan stays within 32 KiB above
        // the current frame, inside the interrupted task's stack or the memory just past it.
        let word = unsafe { (at as *const usize).read() };
        if base != 0 && (base..base + size).contains(&word) {
            frames.push(word - base);
        }
        at += size_of::<usize>();
    }
    log::error!(
        "no main-loop step for {ticks} ms during {}: a task is spinning without yielding",
        ferrite_wifi_start::stage().to_string_lossy()
    );
    log::error!(
        "stall: image base {base:#x}, tick at +{:#x}; code addresses on the stack: {frames:x?}",
        tick as *const () as usize - base
    );
    ferrite_lkpi::pci::park_all();
}

/// The stub that stopped the run, if one has.
pub fn halted() -> Option<&'static CStr> {
    let stub = HALTED.load(Ordering::Acquire);
    // SAFETY: stub names are string literals in the image, so they live as long as it does.
    (!stub.is_null()).then(|| unsafe { CStr::from_ptr(stub) })
}

/// The halt hook. On a task stack it parks the task for good and lets the main loop end the run; on the
/// main stack, where nothing can switch away, it saves the log and exits to the firmware itself.
fn halt(stub: &CStr) -> ! {
    let first = HALTED.swap(stub.as_ptr().cast_mut(), Ordering::AcqRel).is_null();
    log::error!("halted in stub {} during {}", stub.to_string_lossy(), ferrite_wifi_start::stage().to_string_lossy());
    if first {
        ferrite_lkpi::pci::park_all();
    }
    let on_task = ferrite_lkpi::task::lx_task_current() != ferrite_wifi_start::tasks().init.as_ptr();
    if first && on_task {
        loop {
            ferrite_lkpi::task::lx_task_schedule(1);
        }
    }
    let _ = logger::with(BootLog::flush);
    // SAFETY: nothing owned by the run is used again, and the image never returns.
    let _ = unsafe { boot::exit(boot::image_handle(), EXIT_HALTED, 0, core::ptr::null_mut()) };
    loop {
        core::hint::spin_loop();
    }
}

/// Note where the image is loaded, for the stall report.
fn record_image() {
    let params = OpenProtocolParams { handle: boot::image_handle(), agent: boot::image_handle(), controller: None };
    // SAFETY: a shared open of this image's own LoadedImage, read once and dropped.
    match unsafe { boot::open_protocol::<LoadedImage>(params, OpenProtocolAttributes::GetProtocol) } {
        Ok(image) => {
            let (base, size) = image.info();
            IMAGE_BASE.store(base as usize, Ordering::Relaxed);
            IMAGE_SIZE.store(size as usize, Ordering::Relaxed);
            log::info!("image loaded at {:#x}, {size:#x} bytes", base as usize);
        },
        Err(e) => log::warn!("reading this image's load address failed: {e}; a stall report will lack addresses"),
    }
}

/// The kernel arena, from the first size in `sizes` the firmware has room for, mapped for the card
/// through its PCI I/O when DMA protection is on. If that mapping fails the card is withheld from the
/// drivers, since its DMA would be blocked, and the arena is the identity one.
fn allocate_arena(
    sizes: &[usize],
    path: DmaPath,
    pci: &'static UefiPci,
) -> Option<(ferrite_lkpi::mem::Arena, ArenaPages)> {
    match (path, pci.io(0)) {
        (DmaPath::PciIoMap, Some(io)) => {
            log::info!("DMA protection is on: the kernel arena is mapped for the card through PCI I/O");
            for &size in sizes {
                match arena::allocate_mapped(io, size) {
                    Ok(found) => return Some(found),
                    Err(e) => log::warn!("{e}"),
                }
            }
            log::error!("no kernel arena could be mapped for the card; the card is withheld from the drivers");
            pci.withhold();
        },
        (DmaPath::PciIoMap, None) => {
            log::info!("DMA protection is on, but there is no card to map for: identity kernel arena");
        },
        (DmaPath::Identity, _) => log::info!("no DMA protection found: identity kernel arena"),
    }
    identity_arena(sizes)
}

fn identity_arena(sizes: &[usize]) -> Option<(ferrite_lkpi::mem::Arena, ArenaPages)> {
    for (index, &size) in sizes.iter().enumerate() {
        match arena::allocate(size) {
            Ok(found) => {
                if index > 0 {
                    log::warn!("kernel arena of {} MiB, smaller than the {} MiB wanted", size >> 20, sizes[0] >> 20);
                }
                log::info!("kernel arena: {} MiB below 4 GiB, bus address = address", size >> 20);
                return Some(found);
            },
            Err(e) => log::warn!("{e}"),
        }
    }
    log::error!("no kernel arena could be allocated");
    None
}

/// The firmware memory and event a runtime holds, returned when it goes: with the cards taken off the
/// bus first, so nothing DMAs into the arena once it is the firmware's again.
struct Resources {
    timer: Option<Event>,
    pages: Option<ArenaPages>,
}

impl Drop for Resources {
    fn drop(&mut self) {
        ferrite_lkpi::pci::park_all();
        if let Some(timer) = self.timer.take()
            && let Err(e) = boot::close_event(timer)
        {
            log::debug!("closing the timer event failed: {e}");
        }
        if let Some(pages) = self.pages.take() {
            // SAFETY: the scheduler and its tasks drop before this field, the cards are parked and no
            // firmware event runs kernel code, so nothing touches the arena again.
            unsafe { pages.release() };
        }
    }
}

/// The shim, running. Hand it a first task, poll it, and [`shutdown`](Runtime::shutdown) it.
pub struct Runtime {
    sched: Scheduler<UefiPlatform, PageStacks, NoHooks>,
    clock: Clock,
    began: Instant,
    polled: bool,
    stacks: StackStats,
    /// After `sched`, so the tasks are gone before the arena is.
    _resources: Resources,
}

impl Runtime {
    /// Bring the shim up on `clock`, discovering the PCI cards itself.
    pub fn start(clock: Clock, config: &RuntimeConfig) -> Result<Self, RuntimeError> {
        Self::start_with(clock, config, Box::leak(Box::new(UefiPci::discover())))
    }

    /// Bring the shim up over cards already found, for an app that looked first.
    pub fn start_with(clock: Clock, config: &RuntimeConfig, pci: &'static UefiPci) -> Result<Self, RuntimeError> {
        ferrite_lkpi::set_halt_hook(halt).map_err(|source| RuntimeError::HaltHook { source })?;
        record_image();
        STALL_TICKS.store(config.stall_limit.map_or(u64::MAX, |limit| limit.as_millis() as u64), Ordering::Relaxed);

        let mut seed = [0_u8; ferrite_lkpi::random::SEED_LEN];
        entropy::firmware_bytes(&mut seed).map_err(|source| RuntimeError::Entropy { source })?;
        log::info!("firmware RNG algorithms: {:?}", entropy::firmware_algorithms().unwrap_or_default());
        ferrite_lkpi::random::seed(seed).map_err(|source| RuntimeError::Seed { source })?;

        let path = iommu::probe().decide();
        let (arena, pages) = allocate_arena(config.arena_sizes, path, pci).ok_or(RuntimeError::NoArena)?;
        let mut resources = Resources { timer: None, pages: Some(pages) };
        ferrite_lkpi::mem::install(arena).map_err(|source| RuntimeError::ArenaInstall { source })?;
        ferrite_lkpi::pci::install(pci).map_err(|source| RuntimeError::PciInstall { source })?;
        ferrite_lkpi::firmware::install(&VolumeFirmware).map_err(|source| RuntimeError::FirmwareInstall { source })?;

        let wall = rtc::now().unwrap_or_else(|e| {
            log::warn!("reading the RTC failed: {e}; the kernel's wall clock starts at the Unix epoch");
            Duration::ZERO
        });
        ferrite_lkpi::time::set_boot_wall_clock(wall).map_err(|source| RuntimeError::WallClock { source })?;
        log::info!("wall clock at start: {} s since the Unix epoch", wall.as_secs());

        // SAFETY: the notify function only bumps an atomic.
        let timer =
            unsafe { boot::create_event(EventType::TIMER | EventType::NOTIFY_SIGNAL, Tpl::CALLBACK, Some(tick), None) }
                .and_then(|event| boot::set_timer(&event, TimerTrigger::Periodic(TICK)).map(|()| event))
                .map_err(|source| RuntimeError::Timer { source })?;
        resources.timer = Some(timer);

        let stacks = PageStacks::new(DEFAULT_STACK_SIZE);
        let stack_stats = stacks.stats();
        let sched = Scheduler::new(UefiPlatform::new(clock), stacks, NoHooks);
        ferrite_lkpi::task::install(sched.handle(), ferrite_wifi_start::tasks())
            .map_err(|source| RuntimeError::TaskInstall { source })?;
        ferrite_wifi_start::start_kthreadd().map_err(|code| RuntimeError::Kthreadd { code })?;
        Ok(Self {
            sched,
            clock,
            began: clock.now(),
            polled: ferrite_lkpi::pci::device_count() > 0,
            stacks: stack_stats,
            _resources: resources,
        })
    }

    /// The scheduler's handle, for starting the first task.
    pub fn sched(&self) -> SchedHandle {
        self.sched.handle()
    }

    /// Where the kernel's firmware files come from; the same provider the C loads through.
    pub fn firmware(&self) -> &'static dyn FirmwareProvider {
        &VolumeFirmware
    }

    /// The scheduler state of `task` (a `task_struct` from `start_init` or `start_station`), or `None`
    /// once it has exited.
    pub fn task_state(&self, task: NonNull<c_void>) -> Option<TaskState> {
        let id = self.sched.find_by_user(task.as_ptr() as usize)?;
        self.sched.info(id).map(|info| info.state)
    }

    /// Run tasks for at most `budget`, then return when the Linux side next wants the CPU. It returns
    /// promptly: a requeueing work item or a NAPI repoll cannot hold the caller past the budget. An
    /// error stops the Linux side and takes the cards off the bus.
    pub fn poll(&mut self, budget: Duration) -> Result<NextPoll, RuntimeError> {
        PROGRESS.store(TICKS.load(Ordering::Relaxed).max(1), Ordering::Relaxed);
        let step = self.sched.poll(budget).map_err(|source| {
            ferrite_lkpi::pci::park_all();
            RuntimeError::Sched { source }
        })?;
        Ok(self.next_poll(step))
    }

    fn next_poll(&self, step: Step) -> NextPoll {
        if step.runnable {
            return NextPoll::Now;
        }
        match step.next_deadline {
            Some(at) => {
                let wait = at.saturating_duration_since(self.sched.handle().now());
                NextPoll::After(if self.polled { wait.min(IRQ_POLL) } else { wait })
            },
            None if self.polled => NextPoll::After(IRQ_POLL),
            None => NextPoll::Idle,
        }
    }

    /// Sleep `wait` on the firmware's clock, for an app whose main loop has nothing else to do. The
    /// stall watch is off meanwhile, since the loop is not spinning.
    pub fn idle(&self, wait: Duration) {
        PROGRESS.store(0, Ordering::Relaxed);
        self.clock.delay(wait);
    }

    /// The runtime's clock, which the scheduler's time is measured from.
    pub fn now(&self) -> Instant {
        self.clock.now()
    }

    /// Firmware timer ticks (1 ms) since the runtime started.
    pub fn ticks(&self) -> u64 {
        TICKS.load(Ordering::Relaxed)
    }

    /// End the run: take the cards off the bus, log the stack and interrupt counters, remove the tasks,
    /// close the timer and give the arena back. The task that ran the exitcalls must be done already.
    pub fn shutdown(self) {
        PROGRESS.store(0, Ordering::Relaxed);
        ferrite_lkpi::pci::park_all();
        let stacks = self.stacks.report();
        log::info!(
            "task stacks: {} allocated, {} with a guard page, deepest use {} of {} bytes",
            stacks.allocated,
            stacks.guarded,
            stacks.deepest,
            stacks.size,
        );
        let (polls, unhandled) = ferrite_wifi_start::irq_counts();
        log::info!(
            "run took {} ms; {} timer ticks; {} kernel pages free; {polls} polled IRQ handler runs, {unhandled} IRQ_NONE",
            (self.clock.now() - self.began).as_millis(),
            TICKS.load(Ordering::Relaxed),
            ferrite_lkpi::mem::free_page_count().unwrap_or(0),
        );
    }
}
