//! The `lx_task_*` calls the shadow `kernel/sched/core.c` and `kernel/fork.c` make, over the scheduler
//! the app installs.
//!
//! Each kernel thread is one scheduler task whose `user` word is its `task_struct`. The C global
//! `current_task` is written on every switch: the task's own `task_struct` while it runs, `init_task`
//! whenever the main stack has the CPU.

use alloc::string::String;
use core::ffi::{CStr, c_char, c_int, c_void};
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, Ordering};

use snafu::Snafu;

use crate::sched::{Priority, SchedHandle};
use crate::trace_and_stop;

/// The C side's thread entry: a kernel thread function and its argument.
pub type ThreadFn = unsafe extern "sysv64" fn(*mut c_void) -> c_int;

/// Where the C keeps `current`, and the `task_struct` the main stack runs as.
#[derive(Clone, Copy, Debug)]
pub struct LinuxTasks {
    /// The `current_task` variable.
    pub current: NonNull<*mut c_void>,
    /// `init_task`.
    pub init: NonNull<c_void>,
}

pub(crate) struct Installed {
    pub(crate) sched: SchedHandle,
    tasks: LinuxTasks,
}

impl Installed {
    fn set_current(&self, task: *mut c_void) {
        // SAFETY: `current` points at the C variable, which lives as long as the program, and the C
        // only reads it from the one thread that runs every task.
        unsafe { self.tasks.current.as_ptr().write(task) };
    }
}

static INSTALLED: AtomicPtr<Installed> = AtomicPtr::new(core::ptr::null_mut());

#[derive(Debug, Snafu)]
#[snafu(display("a scheduler is already installed"))]
pub struct AlreadyInstalled;

/// Route the `lx_task_*` calls to `sched`, once per program. `current` is set to `init_task`.
pub fn install(sched: SchedHandle, tasks: LinuxTasks) -> Result<(), AlreadyInstalled> {
    let installed = alloc::boxed::Box::into_raw(alloc::boxed::Box::new(Installed { sched, tasks }));
    if INSTALLED.compare_exchange(core::ptr::null_mut(), installed, Ordering::AcqRel, Ordering::Acquire).is_err() {
        // SAFETY: the box was never shared.
        drop(unsafe { alloc::boxed::Box::from_raw(installed) });
        return Err(AlreadyInstalled);
    }
    // SAFETY: just stored, and never freed.
    unsafe { &*installed }.set_current(tasks.init.as_ptr());
    Ok(())
}

pub(crate) fn installed(call: &CStr) -> &'static Installed {
    let raw = INSTALLED.load(Ordering::Acquire);
    if raw.is_null() {
        log::error!("{} called before a scheduler was installed", call.to_string_lossy());
        trace_and_stop(call);
    }
    // SAFETY: set once by `install` from a leaked box, never freed.
    unsafe { &*raw }
}

/// Whether the caller runs on a scheduler task's stack. On the main stack nothing can switch away, so a
/// stop there has nowhere to park; on a task it can park that task and leave the rest running.
pub fn on_task() -> bool {
    let raw = INSTALLED.load(Ordering::Acquire);
    // SAFETY: set once by `install` from a leaked box, never freed.
    !raw.is_null() && unsafe { &*raw }.sched.with_current(|_| ()).is_some()
}

/// A Linux priority (0 to 139) as the scheduler's; out-of-range values clamp.
fn priority(prio: c_int) -> Priority {
    let level = u8::try_from(prio.clamp(0, c_int::from(Priority::LOWEST.get()))).unwrap_or(Priority::LOWEST.get());
    Priority::new(level).unwrap_or(Priority::LOWEST)
}

/// Create a task running `func(arg)` as `task`, queued to run. Returns 0, or -1 if no stack was had.
///
/// # Safety
/// `task` must be a `task_struct` that outlives the task, `func` a kernel thread function, and `name`
/// null or a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "sysv64" fn lx_task_create(
    task: *mut c_void,
    func: ThreadFn,
    arg: *mut c_void,
    name: *const c_char,
    prio: c_int,
) -> c_int {
    let lx = installed(c"lx_task_create");
    let name: String = if name.is_null() {
        "kthread".into()
    } else {
        // SAFETY: the caller guarantees a NUL-terminated string.
        unsafe { CStr::from_ptr(name) }.to_string_lossy().into_owned()
    };
    let arg = arg as usize;
    let spawned = lx.sched.spawn(name, priority(prio), task as usize, move |_| {
        lx.set_current(task);
        // SAFETY: the caller of `lx_task_create` vouched for `func` and its argument.
        let code = unsafe { func(core::ptr::with_exposed_provenance_mut(arg)) };
        log::debug!("kernel thread {task:p} returned {code}");
        lx.set_current(lx.tasks.init.as_ptr());
    });
    match spawned {
        Ok(_) => 0,
        Err(error) => {
            log::error!("creating a kernel thread failed: {error}");
            -1
        },
    }
}

/// Give up the CPU from the running task; with `block` it stays parked until `lx_task_unblock`.
#[unsafe(no_mangle)]
pub extern "sysv64" fn lx_task_schedule(block: c_int) {
    let lx = installed(c"lx_task_schedule");
    let ran = lx.sched.with_current(|task| {
        lx.set_current(lx.tasks.init.as_ptr());
        task.schedule(block != 0);
        lx.set_current(core::ptr::with_exposed_provenance_mut(task.user()));
    });
    if ran.is_none() {
        log::error!("schedule() called on the main stack, where nothing can switch back");
        trace_and_stop(c"lx_task_schedule");
    }
}

/// Make the task of `task` runnable. A `task_struct` no task runs as (`init_task`) is ignored.
#[unsafe(no_mangle)]
pub extern "sysv64" fn lx_task_unblock(task: *mut c_void) {
    let lx = installed(c"lx_task_unblock");
    match lx.sched.find_by_user(task as usize) {
        Some(id) => {
            lx.sched.unblock(id);
        },
        None => log::debug!("unblock of {task:p}, which no task runs as"),
    }
}

/// The running task's `task_struct`, `init_task` on the main stack.
#[unsafe(no_mangle)]
pub extern "sysv64" fn lx_task_current() -> *mut c_void {
    let lx = installed(c"lx_task_current");
    // SAFETY: as in `set_current`.
    unsafe { lx.tasks.current.as_ptr().read() }
}

/// Set the priority of the task of `task`, a Linux priority (0 to 139).
#[unsafe(no_mangle)]
pub extern "sysv64" fn lx_task_priority(task: *mut c_void, prio: c_int) {
    let lx = installed(c"lx_task_priority");
    if let Some(id) = lx.sched.find_by_user(task as usize)
        && let Err(error) = lx.sched.set_priority(id, priority(prio))
    {
        log::debug!("{error}");
    }
}

/// End the running task; its frames are abandoned. On the main stack this stops the program.
#[unsafe(no_mangle)]
pub extern "sysv64" fn lx_task_exit() -> ! {
    let lx = installed(c"lx_task_exit");
    lx.sched.with_current(|task| {
        lx.set_current(lx.tasks.init.as_ptr());
        task.exit()
    });
    log::error!("do_exit() called on the main stack");
    trace_and_stop(c"lx_task_exit")
}
