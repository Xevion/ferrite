//! The Linux C the lab drives directly: thread creation, and the kthread round trip that checks the
//! scheduler seam before the initcalls run.

use std::ffi::{CStr, c_char, c_int, c_uint, c_ulong, c_void};
use std::fmt;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

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
    static jiffies: c_ulong;
    fn msleep(msecs: c_uint);
    fn __msecs_to_jiffies(msecs: c_uint) -> c_ulong;
    fn timer_init_key(timer: *mut TimerList, func: TimerFn, flags: c_uint, name: *const c_char, key: *mut c_void);
    fn mod_timer(timer: *mut TimerList, expires: c_ulong) -> c_int;
    fn timer_delete_sync(timer: *mut TimerList) -> c_int;
    fn request_firmware(firmware: *mut *const Firmware, name: *const c_char, device: *mut c_void) -> c_int;
    fn release_firmware(firmware: *const Firmware);
    fn rtnl_lock();
    fn rtnl_unlock();
    fn freq_reg_info(wiphy: *mut c_void, center_freq_khz: u32) -> *const c_void;
}

/// `struct firmware`.
#[repr(C)]
struct Firmware {
    size: usize,
    data: *const u8,
    private: *mut c_void,
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

type TimerFn = unsafe extern "C" fn(*mut TimerList);

/// Storage for a `struct timer_list` (40 bytes in this config), with room to spare; only the C reads it.
#[repr(C, align(8))]
pub struct TimerList([u64; 8]);

/// How long the time check sleeps, and how far ahead its timer is due.
pub const SLEEP_MS: c_uint = 20;
const TIMER_MS: c_uint = 10;

static TIMER_FIRED: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn timer_fired(_: *mut TimerList) {
    TIMER_FIRED.store(true, Ordering::Relaxed);
}

#[derive(Debug)]
pub enum TimeError {
    Short(Duration),
    TimerNeverFired,
}

impl fmt::Display for TimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Short(slept) => write!(f, "msleep({SLEEP_MS}) returned after {} us", slept.as_micros()),
            Self::TimerNeverFired => write!(f, "a timer_list due in {TIMER_MS} ms had not fired after the sleep"),
        }
    }
}

/// `msleep` from a kernel thread must take at least as long in host time, and a `timer_list` due before
/// it ends must have fired on the timer task.
pub fn sleep_and_timer() -> Result<Duration, TimeError> {
    let mut timer = TimerList([0; 8]);
    // SAFETY: called on a task after the time bring-up; the timer is deleted before its storage goes.
    let slept = unsafe {
        timer_init_key(&raw mut timer, timer_fired, 0, c"lab-timer".as_ptr(), std::ptr::null_mut());
        mod_timer(&raw mut timer, jiffies.wrapping_add(__msecs_to_jiffies(TIMER_MS)));
        let start = Instant::now();
        msleep(SLEEP_MS);
        let slept = start.elapsed();
        timer_delete_sync(&raw mut timer);
        slept
    };
    if slept < Duration::from_millis(u64::from(SLEEP_MS)) {
        return Err(TimeError::Short(slept));
    }
    if TIMER_FIRED.load(Ordering::Relaxed) { Ok(slept) } else { Err(TimeError::TimerNeverFired) }
}

const ENOENT: c_int = 2;
const REGDB_POLL_MS: c_uint = 10;

#[derive(Debug)]
pub enum FirmwareError {
    Read(std::io::Error),
    Request(c_int),
    Size { got: usize, want: usize },
    Bytes,
    MissingFile(c_int),
}

impl fmt::Display for FirmwareError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(error) => write!(f, "reading the reference file failed: {error}"),
            Self::Request(code) => write!(f, "request_firmware failed: {code}"),
            Self::Size { got, want } => write!(f, "request_firmware returned {got} bytes, the file has {want}"),
            Self::Bytes => write!(f, "request_firmware returned bytes that differ from the file"),
            Self::MissingFile(code) => write!(f, "a request for a missing file returned {code}, not -{ENOENT}"),
        }
    }
}

/// `request_firmware` of `name` must return the bytes of the same file below `root`, and of a name
/// that is not there, -ENOENT. Returns the size.
pub fn firmware_round_trip(root: &Path, name: &CStr) -> Result<usize, FirmwareError> {
    let want = std::fs::read(root.join(name.to_string_lossy().as_ref())).map_err(FirmwareError::Read)?;
    let mut firmware: *const Firmware = std::ptr::null();
    // SAFETY: called on a task with the provider installed; a null device is allowed.
    let code = unsafe { request_firmware(&raw mut firmware, name.as_ptr(), std::ptr::null_mut()) };
    if code != 0 {
        return Err(FirmwareError::Request(code));
    }
    // SAFETY: the call succeeded, so `firmware` is a live `struct firmware` until released.
    let got = unsafe { std::slice::from_raw_parts((*firmware).data, (*firmware).size) };
    let result = if got.len() != want.len() {
        Err(FirmwareError::Size { got: got.len(), want: want.len() })
    } else if got != want {
        Err(FirmwareError::Bytes)
    } else {
        Ok(got.len())
    };
    // SAFETY: the pointer came from the successful request above and is released once.
    unsafe { release_firmware(firmware) };
    let size = result?;

    let mut missing: *const Firmware = std::ptr::null();
    // SAFETY: as above.
    let code =
        unsafe { request_firmware(&raw mut missing, c"ferrite-lab-no-such-file.bin".as_ptr(), std::ptr::null_mut()) };
    if code != -ENOENT || !missing.is_null() {
        return Err(FirmwareError::MissingFile(code));
    }
    Ok(size)
}

/// Whether the regulatory domain in force has a rule for 868 MHz. cfg80211's built-in world domain
/// starts at 2.4 GHz; the "00" domain in `regulatory.db` adds 755 to 928 MHz.
fn regdomain_has_sub_ghz_rule() -> bool {
    /// Room for a `struct wiphy`, whose `regd` is NULL when zeroed, so the global domain answers.
    #[repr(C, align(16))]
    struct Wiphy([u8; 8192]);
    let mut wiphy = Wiphy([0; 8192]);
    // SAFETY: called on a task after cfg80211_init; the zeroed wiphy has no regd of its own.
    let rule = unsafe {
        rtnl_lock();
        let rule = freq_reg_info((&raw mut wiphy).cast(), 868_000);
        rtnl_unlock();
        rule
    };
    (rule as usize) < usize::MAX - MAX_ERRNO
}

#[derive(Debug)]
pub enum RegdbError {
    AlreadyLoaded,
    NotLoaded(Duration),
}

impl fmt::Display for RegdbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyLoaded => write!(f, "the regulatory domain had the database's rule before it was loaded"),
            Self::NotLoaded(waited) => {
                write!(f, "the regulatory domain had no database rule after {} ms", waited.as_millis())
            },
        }
    }
}

/// cfg80211 asks for `regulatory.db` from a work item once its init has run. Sleep on this task so
/// the scheduler runs that work, until the domain in force has the database's rule; returns how long.
pub fn wait_for_regdb(limit: Duration) -> Result<Duration, RegdbError> {
    if regdomain_has_sub_ghz_rule() {
        return Err(RegdbError::AlreadyLoaded);
    }
    let start = Instant::now();
    while start.elapsed() < limit {
        // SAFETY: called on a task after the time bring-up.
        unsafe { msleep(REGDB_POLL_MS) };
        if regdomain_has_sub_ghz_rule() {
            return Ok(start.elapsed());
        }
    }
    Err(RegdbError::NotLoaded(start.elapsed()))
}
