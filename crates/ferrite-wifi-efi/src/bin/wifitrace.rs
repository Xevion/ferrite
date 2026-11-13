//! `wifitrace`: the Linux Wi-Fi C under UEFI, started the way `ferrite-lab` starts it on the host.
//!
//! The kernel arena, task stacks, random seed, firmware files and wall clock come from firmware services
//! (`ferrite-wifi-efi`); the C runs on the shim's scheduler as kernel threads, polled from the main stack.
//! Init runs the seam checks, every initcall, the firmware check and every exitcall (`ferrite-wifi-start`),
//! and the app exits to the firmware or the loader. A 1 ms firmware timer event counts ticks, so firmware
//! interrupts keep landing on task stacks; its notify only bumps a counter and never runs Linux code.
//!
//! The lab's no-access fence over unimplemented kernel data has no UEFI counterpart yet: here that data is
//! plain zeroed storage in its own section.

#![no_std]
#![no_main]

use core::ffi::{CStr, c_char, c_void};
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use core::time::Duration;

use ferrite_efi::bootlog::{BootLog, Echo};
use ferrite_efi::clock::Clock;
use ferrite_efi::{entropy, logger, rtc, session};
use ferrite_hal::{Clock as _, Delay as _};
use ferrite_lkpi::sched::{DEFAULT_STACK_SIZE, NoHooks, Scheduler, Step};
use ferrite_wifi_efi::arena;
use ferrite_wifi_efi::firmware::VolumeFirmware;
use ferrite_wifi_efi::platform::UefiPlatform;
use ferrite_wifi_efi::stacks::PageStacks;
use ferrite_wifi_start::StartUp;
use log::LevelFilter;
use uefi::boot::{self, EventType, TimerTrigger, Tpl};
use uefi::prelude::*;

/// Kernel memory, with fallbacks for firmware that cannot spare a 4 MiB-aligned range below 4 GiB of the
/// first size. The run uses a small fraction of 64 MiB; rtw89 and iwlwifi buffers fit in it with room.
const ARENA_SIZES: [usize; 3] = [64 << 20, 32 << 20, 16 << 20];
const TICK: Duration = Duration::from_millis(1);
/// How long one poll step may run tasks before the main loop gets the CPU back.
const BUDGET: Duration = Duration::from_millis(10);
/// How long cfg80211 gets to load `regulatory.db` after its initcalls; FAT reads on real media are slow.
const REGDB_LIMIT: Duration = Duration::from_secs(5);
const EXIT_AFTER: Duration = Duration::from_secs(10);
/// Exit status of a run that stopped in a stub: an OEM error code, distinct from every standard one.
const EXIT_HALTED: Status = Status(Status::ERROR_BIT | 0x4000_0000_0000_0000 | 0x56);

static TICKS: AtomicU64 = AtomicU64::new(0);
/// The stub that stopped the run, null until one does.
static HALTED: AtomicPtr<c_char> = AtomicPtr::new(core::ptr::null_mut());

unsafe extern "efiapi" fn tick(_: uefi::Event, _: Option<NonNull<c_void>>) {
    TICKS.fetch_add(1, Ordering::Relaxed);
}

fn halted() -> Option<&'static CStr> {
    let stub = HALTED.load(Ordering::Acquire);
    // SAFETY: stub names are string literals in the image, so they live as long as it does.
    (!stub.is_null()).then(|| unsafe { CStr::from_ptr(stub) })
}

/// The halt hook. On a task stack it parks the task for good and lets the main loop end the run; on the
/// main stack, where nothing can switch away, it saves the log and exits to the firmware itself.
fn halt(stub: &CStr) -> ! {
    let first = HALTED.swap(stub.as_ptr().cast_mut(), Ordering::AcqRel).is_null();
    log::error!("halted in stub {}", stub.to_string_lossy());
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

/// Install the shim's platform pieces, then run the stack.
fn run() -> Status {
    if let Err(e) = ferrite_lkpi::set_halt_hook(halt) {
        log::error!("{e}");
        return Status::ABORTED;
    }
    let started = Clock::start();

    let mut seed = [0_u8; ferrite_lkpi::random::SEED_LEN];
    if let Err(e) = entropy::firmware_bytes(&mut seed) {
        log::error!("refusing to start without firmware entropy: {e}");
        return Status::UNSUPPORTED;
    }
    log::info!("firmware RNG algorithms: {:?}", entropy::firmware_algorithms().unwrap_or_default());
    if let Err(e) = ferrite_lkpi::random::seed(seed) {
        log::error!("{e}");
        return Status::ABORTED;
    }

    let Some((arena, pages)) = allocate_arena() else { return Status::OUT_OF_RESOURCES };
    if let Err(e) = ferrite_lkpi::mem::install(arena) {
        log::error!("{e}");
        return Status::ABORTED;
    }

    if let Err(e) = ferrite_lkpi::firmware::install(&VolumeFirmware) {
        log::error!("{e}");
        return Status::ABORTED;
    }

    let wall = rtc::now().unwrap_or_else(|e| {
        log::warn!("reading the RTC failed: {e}; the kernel's wall clock starts at the Unix epoch");
        Duration::ZERO
    });
    if let Err(e) = ferrite_lkpi::time::set_boot_wall_clock(wall) {
        log::error!("{e}");
        return Status::ABORTED;
    }
    log::info!("wall clock at start: {} s since the Unix epoch", wall.as_secs());

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

    let status = execute(started);
    if let Err(e) = boot::close_event(timer) {
        log::debug!("closing the timer event failed: {e}");
    }
    // SAFETY: the scheduler and its tasks are gone and no firmware event runs kernel code, so nothing
    // touches the arena again.
    unsafe { pages.release() };
    status
}

/// The kernel arena, from the first size in `ARENA_SIZES` the firmware has room for.
fn allocate_arena() -> Option<(ferrite_lkpi::mem::Arena, arena::ArenaPages)> {
    for (index, size) in ARENA_SIZES.into_iter().enumerate() {
        match arena::allocate(size) {
            Ok(found) => {
                if index > 0 {
                    log::warn!(
                        "kernel arena of {} MiB, smaller than the {} MiB wanted",
                        size >> 20,
                        ARENA_SIZES[0] >> 20
                    );
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

/// Start kthreadd and init on a scheduler and poll it until init is gone.
fn execute(clock: Clock) -> Status {
    let stacks = PageStacks::new(DEFAULT_STACK_SIZE);
    let stack_stats = stacks.stats();
    let mut sched = Scheduler::new(UefiPlatform::new(clock), stacks, NoHooks);
    if let Err(e) = ferrite_lkpi::task::install(sched.handle(), ferrite_wifi_start::tasks()) {
        log::error!("{e}");
        return Status::ABORTED;
    }
    if let Err(code) = ferrite_wifi_start::start_kthreadd() {
        log::error!("starting kthreadd failed: {code}");
        return Status::ABORTED;
    }
    let start_up = StartUp { sched: sched.handle(), firmware_reference: &VolumeFirmware, regdb_limit: REGDB_LIMIT };
    let init_task = match ferrite_wifi_start::start_init(start_up) {
        Ok(task) => task,
        Err(code) => {
            log::error!("starting init failed: {code}");
            return Status::ABORTED;
        },
    };
    let Some(init_id) = sched.find_by_user(init_task.as_ptr() as usize) else {
        log::error!("init has no scheduler task");
        return Status::ABORTED;
    };

    let began = clock.now();
    while sched.info(init_id).is_some() && halted().is_none() {
        match sched.poll(BUDGET) {
            Ok(step) if step.runnable || sched.info(init_id).is_none() => {},
            Ok(Step { next_deadline: Some(at), .. }) => {
                clock.delay(at.saturating_duration_since(sched.handle().now()));
            },
            Ok(_) => {
                log::error!("every task is blocked and nothing will wake one; init never finished");
                return Status::ABORTED;
            },
            Err(e) => {
                log::error!("{e}");
                return Status::ABORTED;
            },
        }
    }

    let stacks = stack_stats.report();
    log::info!(
        "task stacks: {} allocated, {} with a guard page, deepest use {} of {} bytes",
        stacks.allocated,
        stacks.guarded,
        stacks.deepest,
        stacks.size,
    );
    log::info!(
        "run took {} ms; {} timer ticks; {} kernel pages free",
        (clock.now() - began).as_millis(),
        TICKS.load(Ordering::Relaxed),
        ferrite_lkpi::mem::free_page_count().unwrap_or(0),
    );
    match halted() {
        Some(stub) => {
            log::error!("the run stopped in stub {}", stub.to_string_lossy());
            EXIT_HALTED
        },
        None if ferrite_wifi_start::init_failed() => Status::ABORTED,
        None => Status::SUCCESS,
    }
}
