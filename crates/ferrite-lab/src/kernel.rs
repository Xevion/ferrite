//! The Linux C the lab drives directly: thread creation, and the kthread round trip that checks the
//! scheduler seam before the initcalls run.

use std::ffi::{CStr, c_char, c_int, c_ulong, c_void};
use std::fmt;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};

use ferrite_lkpi::task::LinuxTasks;

type ThreadFn = unsafe extern "C" fn(*mut c_void) -> c_int;

const NUMA_NO_NODE: c_int = -1;
const MAX_ERRNO: usize = 4095;
const CLONE_FS: c_ulong = 0x200;
const CLONE_FILES: c_ulong = 0x400;
/// What the probe thread returns, and so what `kthread_stop` must report.
const PROBE_RESULT: c_int = 7;

unsafe extern "C" {
    static mut current_task: *mut c_void;
    static mut init_task: [u8; 0];
    static mut kthreadd_task: *mut c_void;

    fn kernel_thread(func: ThreadFn, arg: *mut c_void, name: *const c_char, flags: c_ulong) -> c_int;
    fn kthreadd(arg: *mut c_void) -> c_int;
    fn find_task_by_pid_ns(pid: c_int, ns: *mut c_void) -> *mut c_void;
    fn kthread_create_on_node(func: ThreadFn, data: *mut c_void, node: c_int, fmt: *const c_char, ...) -> *mut c_void;
    fn kthread_should_stop() -> bool;
    fn kthread_stop(task: *mut c_void) -> c_int;
    fn wake_up_process(task: *mut c_void) -> c_int;
    fn schedule();
    fn lx_kernel_init();
}

/// Where the C keeps `current`, and `init_task`.
pub fn tasks() -> LinuxTasks {
    let current = NonNull::new(&raw mut current_task).unwrap_or(NonNull::dangling());
    let init = NonNull::new((&raw mut init_task).cast()).unwrap_or(NonNull::dangling());
    LinuxTasks { current, init }
}

/// The start-up that runs before the initcalls: workqueues, then the driver core.
pub fn kernel_init() {
    // SAFETY: called on the init task, with the scheduler installed and kthreadd running, once.
    unsafe { lx_kernel_init() };
}

/// Create a kernel thread from the main stack and return its `task_struct`.
pub fn spawn(func: ThreadFn, name: &CStr, flags: c_ulong) -> Result<NonNull<c_void>, c_int> {
    // SAFETY: the scheduler is installed, `func` is a thread function and `name` outlives the call.
    let pid = unsafe { kernel_thread(func, std::ptr::null_mut(), name.as_ptr(), flags) };
    if pid < 0 {
        return Err(pid);
    }
    // SAFETY: walks the task list, which only the C changes and nothing else runs meanwhile.
    NonNull::new(unsafe { find_task_by_pid_ns(pid, std::ptr::null_mut()) }).ok_or(pid)
}

/// Start kthreadd, as `rest_init` does, so `kthread_create` has someone to ask.
pub fn start_kthreadd() -> Result<(), c_int> {
    let task = spawn(kthreadd, c"kthreadd", CLONE_FS | CLONE_FILES)?;
    // SAFETY: set once from the main stack before any task runs, as the kernel does.
    unsafe { kthreadd_task = task.as_ptr() };
    Ok(())
}

static PROBE_RAN: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn probe(_: *mut c_void) -> c_int {
    PROBE_RAN.store(true, Ordering::Relaxed);
    // SAFETY: runs on a kthread, which is what both calls expect.
    while !unsafe { kthread_should_stop() } {
        // SAFETY: the task is TASK_RUNNING, so this only yields.
        unsafe { schedule() };
    }
    PROBE_RESULT
}

#[derive(Debug)]
pub enum ProbeError {
    Create(isize),
    NeverRan,
    Result(c_int),
}

impl fmt::Display for ProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Create(code) => write!(f, "kthread_create failed: {code}"),
            Self::NeverRan => write!(f, "the woken thread did not run when init yielded"),
            Self::Result(code) => write!(f, "kthread_stop returned {code}, not {PROBE_RESULT}"),
        }
    }
}

/// Run a thread through kthreadd with `kthread_run` and stop it with `kthread_stop`: creation, wakeups,
/// completions, yields and exit, all over the scheduler.
pub fn kthread_round_trip() -> Result<(), ProbeError> {
    // SAFETY: called on a task, with kthreadd started; the format takes no arguments.
    let task = unsafe { kthread_create_on_node(probe, std::ptr::null_mut(), NUMA_NO_NODE, c"lab-probe".as_ptr()) };
    if task.is_null() || task as usize > usize::MAX - MAX_ERRNO {
        return Err(ProbeError::Create(task as isize));
    }
    // SAFETY: a live task_struct kthread_create returned; yielding lets the woken thread run.
    unsafe {
        wake_up_process(task);
        schedule();
    }
    if !PROBE_RAN.load(Ordering::Relaxed) {
        return Err(ProbeError::NeverRan);
    }
    // SAFETY: the thread only exits once told to, so the task_struct is still live.
    let code = unsafe { kthread_stop(task) };
    if code == PROBE_RESULT { Ok(()) } else { Err(ProbeError::Result(code)) }
}
