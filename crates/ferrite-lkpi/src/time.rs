//! The `lx_time_*` calls the shadow `arch/x86/kernel/time.c` makes: the kernel's clock and its one
//! oneshot timer over the installed scheduler, and the boot wall clock the app passes in.
//!
//! The clock counts nanoseconds of the scheduler platform's `now()`. Arming the clockevent sets the
//! scheduler's single deadline, and the scheduler wakes the designated timer task once it passes.

use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use core::time::Duration;

use snafu::Snafu;

use crate::task::installed;
use crate::trace_and_stop;

static BOOT_WALL_NS: AtomicU64 = AtomicU64::new(0);
static BOOT_WALL_SET: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Snafu)]
#[snafu(display("the boot wall clock is already set"))]
pub struct WallClockAlreadySet;

/// Set the wall clock at start-up, as time since the Unix epoch, once per program.
pub fn set_boot_wall_clock(since_unix_epoch: Duration) -> Result<(), WallClockAlreadySet> {
    if BOOT_WALL_SET.load(Ordering::Acquire) {
        return Err(WallClockAlreadySet);
    }
    BOOT_WALL_NS.store(nanos(since_unix_epoch), Ordering::Relaxed);
    BOOT_WALL_SET.store(true, Ordering::Release);
    Ok(())
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// The kernel clock: nanoseconds of the scheduler platform's clock.
#[unsafe(no_mangle)]
pub extern "sysv64" fn lx_time_counter() -> u64 {
    nanos(installed(c"lx_time_counter").sched.now().since_epoch())
}

/// Arm the oneshot timer `delta_ns` from now.
#[unsafe(no_mangle)]
pub extern "sysv64" fn lx_time_arm(delta_ns: u64) {
    let sched = &installed(c"lx_time_arm").sched;
    let at = sched.now().checked_add(Duration::from_nanos(delta_ns));
    sched.arm_timer(at);
}

/// Cancel the oneshot timer.
#[unsafe(no_mangle)]
pub extern "sysv64" fn lx_time_stop() {
    installed(c"lx_time_stop").sched.arm_timer(None);
}

/// Make the task of `task` the one woken when the timer fires.
#[unsafe(no_mangle)]
pub extern "sysv64" fn lx_time_set_timer_task(task: *mut c_void) {
    let sched = &installed(c"lx_time_set_timer_task").sched;
    let Some(id) = sched.find_by_user(task as usize) else {
        log::error!("timer task {task:p} has no scheduler task");
        trace_and_stop(c"lx_time_set_timer_task");
    };
    sched.set_timer_task(id);
}

/// The wall clock at start-up in nanoseconds since the Unix epoch. Stops if the app never set it.
#[unsafe(no_mangle)]
pub extern "sysv64" fn lx_time_boot_wall_ns() -> u64 {
    if !BOOT_WALL_SET.load(Ordering::Acquire) {
        log::error!("the kernel read the wall clock before the app set it");
        trace_and_stop(c"lx_time_boot_wall_ns");
    }
    BOOT_WALL_NS.load(Ordering::Relaxed)
}
