//! The Wi-Fi C's start-up, shared by the lab and the UEFI image: the C entry points the programs call
//! directly, and the init thread that checks each seam before it runs the initcalls and exitcalls.
//!
//! A program installs the shim (scheduler, arena, random seed, firmware provider, PCI devices, wall clock), starts
//! kthreadd with [`start_kthreadd`] and the init thread with [`start_init`], then polls its scheduler
//! until the init task is gone and asks [`init_failed`] how it went. The C is resolved by the program's
//! link, so this crate links only into a program that also links `ferrite-wifi-sys` and `ferrite-lkpi`.

#![no_std]

extern crate alloc;

mod scan;

use alloc::boxed::Box;
use core::ffi::{CStr, c_char, c_int, c_uint, c_ulong, c_void};
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use core::time::Duration;

use ferrite_hal::Instant;
use ferrite_lkpi::firmware::{FirmwareError, FirmwareProvider};
use ferrite_lkpi::sched::SchedHandle;
use ferrite_lkpi::task::LinuxTasks;
use ferrite_wifi_sys::{exitcalls, initcalls};
use snafu::Snafu;

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
    fn __page_frag_alloc_align(cache: *mut PageFragCache, size: c_uint, gfp: c_uint, align_mask: c_uint) -> *mut u8;
    fn page_frag_free(addr: *mut u8);
    fn page_frag_cache_drain(cache: *mut PageFragCache);
    fn __alloc_skb(size: c_uint, gfp: c_uint, flags: c_int, node: c_int) -> *mut c_void;
    fn sk_skb_reason_drop(sk: *mut c_void, skb: *mut c_void, reason: c_uint);
    fn lx_pci_driver_name(index: c_ulong) -> *const c_char;
    fn lx_wireless_ctl(op: c_int, nth: c_int, name: *mut c_char) -> c_int;
    static lx_irq_polls: c_ulong;
    static lx_irq_unhandled: c_ulong;
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

/// Create a kernel thread from the main stack and return its `task_struct`.
fn spawn(func: ThreadFn, arg: *mut c_void, name: &CStr, flags: c_ulong) -> Result<NonNull<c_void>, c_int> {
    // SAFETY: the scheduler is installed, `func` is a thread function and `name` outlives the call.
    let pid = unsafe { kernel_thread(func, arg, name.as_ptr(), flags) };
    if pid < 0 {
        return Err(pid);
    }
    // SAFETY: walks the task list, which only the C changes and nothing else runs meanwhile.
    NonNull::new(unsafe { find_task_by_pid_ns(pid, core::ptr::null_mut()) }).ok_or(pid)
}

/// Start kthreadd, as `rest_init` does, so `kthread_create` has someone to ask.
pub fn start_kthreadd() -> Result<NonNull<c_void>, c_int> {
    let task = spawn(kthreadd, core::ptr::null_mut(), c"kthreadd", CLONE_FS | CLONE_FILES)?;
    // SAFETY: set once from the main stack before any task runs, as the kernel does.
    unsafe { kthreadd_task = task.as_ptr() };
    Ok(task)
}

/// What the init thread needs from the program that starts it.
pub struct StartUp {
    /// The scheduler the init task runs on, for the clock the checks measure with.
    pub sched: SchedHandle,
    /// Supplies the reference bytes the firmware check compares `request_firmware` against.
    pub firmware_reference: &'static dyn FirmwareProvider,
    /// How long cfg80211 gets to load `regulatory.db` after its initcalls.
    pub regdb_limit: Duration,
    /// How long a PCI Wi-Fi card gets from its probe to a registered wireless interface.
    pub wifi_limit: Duration,
    /// How long a scan gets from its trigger to the scan group announcing its end.
    pub scan_limit: Duration,
}

/// Create the init thread, which runs the checks and every initcall and exitcall, and return its
/// `task_struct` so the program can wait for its task to finish.
pub fn start_init(start_up: StartUp) -> Result<NonNull<c_void>, c_int> {
    let arg = Box::into_raw(Box::new(start_up));
    spawn(init, arg.cast(), c"init", 0).inspect_err(|_| {
        // SAFETY: the thread was not created, so nothing else holds the box.
        drop(unsafe { Box::from_raw(arg) });
    })
}

static INIT_FAILED: AtomicBool = AtomicBool::new(false);

/// Whether the init thread stopped at a failed check, which it has logged.
pub fn init_failed() -> bool {
    INIT_FAILED.load(Ordering::Acquire)
}

/// The step the init thread is in, for a program that stops it from outside.
static STAGE: AtomicPtr<u8> = AtomicPtr::new(c"starting".as_ptr().cast_mut().cast());

fn enter(stage: &'static CStr) {
    STAGE.store(stage.as_ptr().cast_mut().cast(), Ordering::Release);
}

/// The step the init thread last entered.
pub fn stage() -> &'static CStr {
    // SAFETY: only `enter` stores, always a static C string.
    unsafe { CStr::from_ptr(STAGE.load(Ordering::Acquire).cast()) }
}

/// Polled interrupt runs, and runs whose handlers all said `IRQ_NONE`.
pub fn irq_counts() -> (u64, u64) {
    // SAFETY: plain counters the C bumps on the same thread.
    unsafe { (lx_irq_polls, lx_irq_unhandled) }
}

/// The init thread: the seam checks, then every initcall and exitcall.
unsafe extern "C" fn init(arg: *mut c_void) -> c_int {
    // SAFETY: `start_init` passed the pointer of a leaked `StartUp` and nothing else uses it.
    let start_up = unsafe { Box::from_raw(arg.cast::<StartUp>()) };
    if run(&start_up) {
        0
    } else {
        INIT_FAILED.store(true, Ordering::Release);
        1
    }
}

/// Run every step in order; true when all passed. A failed check is logged and stops the run, except
/// that a failed Wi-Fi card bring-up still runs the exitcalls, so its driver is removed.
fn run(start_up: &StartUp) -> bool {
    let clock = || start_up.sched.now();
    enter(c"kthread round trip");
    match kthread_round_trip() {
        Ok(()) => log::info!("kthread round trip through kthreadd: created, ran, stopped"),
        Err(error) => {
            log::error!("kthread round trip failed: {error}");
            return false;
        },
    }
    enter(c"kernel start-up");
    // SAFETY: called on the init task, with the scheduler installed and kthreadd running, once.
    unsafe { lx_kernel_init() };
    match sleep_and_timer(&clock) {
        Ok(slept) => log::info!("msleep({SLEEP_MS}) returned after {} ms; a timer_list fired", slept.as_millis()),
        Err(error) => {
            log::error!("time check failed: {error}");
            return false;
        },
    }
    match page_model() {
        Ok(()) => log::info!(
            "page model: two page frags shared one compound block through its head and it returned on drain; alloc_skb and kfree_skb balanced"
        ),
        Err(error) => {
            log::error!("page model check failed: {error}");
            return false;
        },
    }
    enter(c"initcalls (PCI probes run here)");
    for call in initcalls() {
        log::info!("initcall level {} {}", call.level().to_string_lossy(), call.name().to_string_lossy());
        // SAFETY: init is the only caller, runs the calls one at a time, and the program links the shim.
        let code = unsafe { call.call() };
        log::info!("initcall {} returned {code}", call.name().to_string_lossy());
    }
    enter(c"firmware check");
    match firmware_round_trip(start_up.firmware_reference, REGDB) {
        Ok(size) => {
            log::info!("request_firmware(regulatory.db) returned {size} bytes; a missing file returned -ENOENT");
        },
        Err(error) => {
            log::error!("firmware check failed: {error}");
            return false;
        },
    }
    enter(c"regulatory.db wait");
    match wait_for_regdb(&clock, start_up.regdb_limit) {
        Ok(waited) => log::info!("regulatory.db loaded and applied by cfg80211 after {} ms", waited.as_millis()),
        Err(error) => {
            log::error!("regulatory.db check failed: {error}");
            return false;
        },
    }
    let wifi = pci_wifi(&clock, start_up.wifi_limit, start_up.scan_limit);
    enter(c"exitcalls (driver removal)");
    let mut ran = 0_usize;
    for call in exitcalls() {
        log::info!("exitcall {}", call.name().to_string_lossy());
        // SAFETY: as above; every initcall returned, so each module is set up.
        unsafe { call.call() };
        ran += 1;
    }
    log::info!("exitcalls done: {ran} ran");
    let (polls, unhandled) = irq_counts();
    log::info!("polled IRQs: {polls} handler runs, {unhandled} answered IRQ_NONE");
    enter(c"done");
    wifi
}

const ENODEV: c_int = 19;
const IFNAMSIZ: usize = 16;
const WIFI_POLL_MS: c_uint = 50;

#[derive(Clone, Copy)]
enum WirelessOp {
    Find = 0,
    Up = 1,
    Down = 2,
}

/// Run `op` on the `nth` wireless interface: its name and the C's result.
fn wireless(op: WirelessOp, nth: c_int) -> (alloc::string::String, c_int) {
    let mut name: [c_char; IFNAMSIZ] = [0; IFNAMSIZ];
    // SAFETY: called on a task after the initcalls; `name` has IFNAMSIZ bytes and comes back terminated.
    let code = unsafe { lx_wireless_ctl(op as c_int, nth, name.as_mut_ptr()) };
    // SAFETY: zeroed and written with a terminated copy of at most IFNAMSIZ bytes.
    let name = unsafe { CStr::from_ptr(name.as_ptr()) }.to_string_lossy().into_owned();
    (name, code)
}

fn driver_of(index: usize) -> Option<alloc::string::String> {
    // SAFETY: returns NULL or a driver name that lives as long as the driver is registered.
    let name = unsafe { lx_pci_driver_name(index as c_ulong) };
    // SAFETY: as above, read at once.
    (!name.is_null()).then(|| unsafe { CStr::from_ptr(name) }.to_string_lossy().into_owned())
}

/// Each PCI Wi-Fi card the platform gave the drivers must be bound after the initcalls, and the first
/// must register a wireless interface (its INIT firmware ran and the NVM was read), come up (its
/// RUNTIME firmware is alive) and scan through nl80211, then go down again before the exitcalls remove
/// the driver. With no card, the simulated radios run the same scan instead. True when every step
/// passed; each step is logged.
fn pci_wifi(clock: &dyn Fn() -> Instant, limit: Duration, scan_limit: Duration) -> bool {
    let count = ferrite_lkpi::pci::device_count();
    if count == 0 {
        log::info!("no PCI Wi-Fi card was handed to the drivers");
        return simulated_wifi(clock, scan_limit);
    }
    let bound: alloc::vec::Vec<Option<alloc::string::String>> = (0..count).map(driver_of).collect();
    for (index, driver) in bound.iter().enumerate() {
        match driver {
            Some(driver) => log::info!("PCI device {index}: probed and bound to {driver}"),
            None => {
                log::error!(
                    "PCI device {index}: no driver bound after the initcalls (probe failed or nothing matched)"
                );
            },
        }
    }
    if bound.iter().any(Option::is_none) {
        return false;
    }
    enter(c"waiting for the wireless interface (firmware load, INIT image, NVM)");
    let start = clock();
    let name = loop {
        let (name, code) = wireless(WirelessOp::Find, 0);
        if code == 0 {
            break name;
        }
        if code != -ENODEV {
            log::error!("looking for the wireless interface failed: {code}");
            return false;
        }
        if driver_of(0).is_none() {
            log::error!(
                "PCI device 0: the driver unbound after {} ms (firmware load or start failed)",
                (clock() - start).as_millis()
            );
            return false;
        }
        if clock() - start >= limit {
            log::error!("no wireless interface after {} ms; the driver is still bound", (clock() - start).as_millis());
            return false;
        }
        // SAFETY: on a task after the time bring-up.
        unsafe { msleep(WIFI_POLL_MS) };
    };
    log::info!(
        "wireless interface {name} registered after {} ms: INIT firmware ran and the NVM was read",
        (clock() - start).as_millis()
    );
    enter(c"bringing the wireless interface up (RUNTIME image)");
    let up_start = clock();
    let (_, code) = wireless(WirelessOp::Up, 0);
    if code != 0 {
        log::error!(
            "dev_open({name}) returned {code} after {} ms: the RUNTIME firmware did not come up",
            (clock() - up_start).as_millis()
        );
        return false;
    }
    log::info!("{name} is up after {} ms: the RUNTIME firmware is alive", (clock() - up_start).as_millis());
    enter(c"scanning through nl80211");
    let scanned = match scan::connect().and_then(|mut client| scan::scan(&mut client, &name, &[], scan_limit, clock)) {
        Ok(_) => true,
        Err(error) => {
            log::error!("nl80211 scan failed: {error}");
            false
        },
    };
    enter(c"bringing the wireless interface down");
    let (_, code) = wireless(WirelessOp::Down, 0);
    log::info!("{name} down ({code})");
    scanned && code == 0
}

/// The SSID the lab's second simulated radio beacons, which the first one's scan must find.
const LAB_SSID: &[u8] = b"ferrite-lab";

/// The wireless interfaces present without a PCI card: none on a real machine, the lab's two
/// `mac80211_hwsim` radios otherwise. The second becomes an open AP, the first scans for it on
/// 2.4 GHz channel 1, and both go down again.
fn simulated_wifi(clock: &dyn Fn() -> Instant, scan_limit: Duration) -> bool {
    let (first, code) = wireless(WirelessOp::Find, 0);
    if code == -ENODEV {
        log::info!("no wireless interface is registered; the nl80211 scan is skipped");
        return true;
    }
    let (second, code) = wireless(WirelessOp::Find, 1);
    if code != 0 {
        log::error!("the simulated radios registered only {first}");
        return false;
    }
    enter(c"scanning simulated radios through nl80211");
    let result = scan::connect().and_then(|mut client| {
        scan::start_ap(&mut client, &second, LAB_SSID, || match wireless(WirelessOp::Up, 1).1 {
            0 => Ok(()),
            code => Err(code),
        })?;
        let (_, code) = wireless(WirelessOp::Up, 0);
        if code != 0 {
            log::error!("dev_open({first}) returned {code}");
        }
        let channel_1 = [ferrite_nl80211::nl80211::Frequency(2412)];
        scan::scan(&mut client, &first, &channel_1, scan_limit, clock)
    });
    for nth in [0, 1] {
        let (name, code) = wireless(WirelessOp::Down, nth);
        log::info!("{name} down ({code})");
    }
    match result {
        Ok(report) if report.bsses.iter().any(|bss| bss.ssid == LAB_SSID) => {
            log::info!(
                "the scan on {first} found {second}'s network \"{}\" after {} ms",
                ferrite_nl80211::nl80211::SsidDisplay(LAB_SSID),
                report.took.as_millis()
            );
            true
        },
        Ok(_) => {
            log::error!("the scan on {first} did not find {second}'s network");
            false
        },
        Err(error) => {
            log::error!("nl80211 scan failed: {error}");
            false
        },
    }
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

#[derive(Debug, Snafu)]
enum ProbeError {
    #[snafu(display("kthread_create failed: {code}"))]
    Create { code: isize },
    #[snafu(display("the woken thread did not run when init yielded"))]
    NeverRan,
    #[snafu(display("kthread_stop returned {code}, not {PROBE_RESULT}"))]
    StopCode { code: c_int },
}

/// Run a thread through kthreadd with `kthread_run` and stop it with `kthread_stop`: creation, wakeups,
/// completions, yields and exit, all over the scheduler.
fn kthread_round_trip() -> Result<(), ProbeError> {
    // SAFETY: called on a task, with kthreadd started; the format takes no arguments.
    let task = unsafe { kthread_create_on_node(probe, core::ptr::null_mut(), NUMA_NO_NODE, c"lab-probe".as_ptr()) };
    if task.is_null() || task as usize > usize::MAX - MAX_ERRNO {
        return Err(ProbeError::Create { code: task as isize });
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
    if code == PROBE_RESULT { Ok(()) } else { Err(ProbeError::StopCode { code }) }
}

type TimerFn = unsafe extern "C" fn(*mut TimerList);

/// Storage for a `struct timer_list` (40 bytes in this config), with room to spare; only the C reads it.
#[repr(C, align(8))]
struct TimerList([u64; 8]);

/// How long the time check sleeps, and how far ahead its timer is due.
const SLEEP_MS: c_uint = 20;
const TIMER_MS: c_uint = 10;

static TIMER_FIRED: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn timer_fired(_: *mut TimerList) {
    TIMER_FIRED.store(true, Ordering::Relaxed);
}

#[derive(Debug, Snafu)]
enum TimeError {
    #[snafu(display("msleep({SLEEP_MS}) returned after {} us", slept.as_micros()))]
    Short { slept: Duration },
    #[snafu(display("a timer_list due in {TIMER_MS} ms had not fired after the sleep"))]
    TimerNeverFired,
}

/// `msleep` from a kernel thread must take at least as long on `clock`, and a `timer_list` due before
/// it ends must have fired on the timer task.
fn sleep_and_timer(clock: &dyn Fn() -> Instant) -> Result<Duration, TimeError> {
    let mut timer = TimerList([0; 8]);
    // SAFETY: called on a task after the time bring-up; the timer is deleted before its storage goes.
    let slept = unsafe {
        timer_init_key(&raw mut timer, timer_fired, 0, c"lab-timer".as_ptr(), core::ptr::null_mut());
        mod_timer(&raw mut timer, jiffies.wrapping_add(__msecs_to_jiffies(TIMER_MS)));
        let start = clock();
        msleep(SLEEP_MS);
        let slept = clock() - start;
        timer_delete_sync(&raw mut timer);
        slept
    };
    if slept < Duration::from_millis(u64::from(SLEEP_MS)) {
        return Err(TimeError::Short { slept });
    }
    if TIMER_FIRED.load(Ordering::Relaxed) { Ok(slept) } else { Err(TimeError::TimerNeverFired) }
}

const ENOENT: c_int = 2;
const REGDB_POLL_MS: c_uint = 10;
/// The file the firmware check loads and cfg80211 asks for.
const REGDB: &CStr = c"regulatory.db";

#[derive(Debug, Snafu)]
enum FirmwareCheckError {
    #[snafu(display("reading the reference file failed: {source}"))]
    Read { source: FirmwareError },
    #[snafu(display("request_firmware failed: {code}"))]
    Request { code: c_int },
    #[snafu(display("request_firmware returned {got} bytes, the file has {want}"))]
    Size { got: usize, want: usize },
    #[snafu(display("request_firmware returned bytes that differ from the file"))]
    Bytes,
    #[snafu(display("a request for a missing file returned {code}, not -{ENOENT}"))]
    MissingFile { code: c_int },
}

/// `request_firmware` of `name` must return the bytes `reference` has for it, and of a name that is not
/// there, -ENOENT. Returns the size.
fn firmware_round_trip(reference: &dyn FirmwareProvider, name: &CStr) -> Result<usize, FirmwareCheckError> {
    let want = reference.load(&name.to_string_lossy()).map_err(|source| FirmwareCheckError::Read { source })?;
    let mut firmware: *const Firmware = core::ptr::null();
    // SAFETY: called on a task with the provider installed; a null device is allowed.
    let code = unsafe { request_firmware(&raw mut firmware, name.as_ptr(), core::ptr::null_mut()) };
    if code != 0 {
        return Err(FirmwareCheckError::Request { code });
    }
    // SAFETY: the call succeeded, so `firmware` is a live `struct firmware` until released.
    let got = unsafe { core::slice::from_raw_parts((*firmware).data, (*firmware).size) };
    let result = if got.len() != want.len() {
        Err(FirmwareCheckError::Size { got: got.len(), want: want.len() })
    } else if got != want.as_slice() {
        Err(FirmwareCheckError::Bytes)
    } else {
        Ok(got.len())
    };
    // SAFETY: the pointer came from the successful request above and is released once.
    unsafe { release_firmware(firmware) };
    let size = result?;

    let mut missing: *const Firmware = core::ptr::null();
    // SAFETY: as above.
    let code =
        unsafe { request_firmware(&raw mut missing, c"ferrite-lab-no-such-file.bin".as_ptr(), core::ptr::null_mut()) };
    if code != -ENOENT || !missing.is_null() {
        return Err(FirmwareCheckError::MissingFile { code });
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

#[derive(Debug, Snafu)]
enum RegdbError {
    #[snafu(display("the regulatory domain had the database's rule before it was loaded"))]
    AlreadyLoaded,
    #[snafu(display("the regulatory domain had no database rule after {} ms", waited.as_millis()))]
    NotLoaded { waited: Duration },
}

/// cfg80211 asks for `regulatory.db` from a work item once its init has run. Sleep on this task so
/// the scheduler runs that work, until the domain in force has the database's rule; returns how long.
fn wait_for_regdb(clock: &dyn Fn() -> Instant, limit: Duration) -> Result<Duration, RegdbError> {
    if regdomain_has_sub_ghz_rule() {
        return Err(RegdbError::AlreadyLoaded);
    }
    let start = clock();
    while clock() - start < limit {
        // SAFETY: called on a task after the time bring-up.
        unsafe { msleep(REGDB_POLL_MS) };
        if regdomain_has_sub_ghz_rule() {
            return Ok(clock() - start);
        }
    }
    Err(RegdbError::NotLoaded { waited: clock() - start })
}

const GFP_KERNEL: c_uint = 0xcc0;
const PAGE_SIZE: usize = 4096;
/// `PAGE_FRAG_CACHE_MAX_ORDER`: a frag cache refills with an order-3 compound block.
const FRAG_BLOCK_PAGES: usize = 8;
/// `SKB_DROP_REASON_NOT_SPECIFIED`, what `kfree_skb` passes.
const SKB_DROP_NOT_SPECIFIED: c_uint = 2;

/// `struct page_frag_cache`: encoded page, offset and bias; zeroed means empty.
#[repr(C)]
struct PageFragCache([u64; 2]);

#[derive(Debug, Snafu)]
enum PageModelError {
    #[snafu(display("page_frag_alloc returned NULL"))]
    FragAlloc,
    #[snafu(display("frags at {first:#x} and {second:#x} are not adjacent pages of one aligned block"))]
    FragLayout { first: usize, second: usize },
    #[snafu(display("the frag cache took {pages} pages, not {FRAG_BLOCK_PAGES}"))]
    Taken { pages: usize },
    #[snafu(display("freeing both frags returned the block while the cache still held it ({missing} pages out)"))]
    FreedEarly { missing: usize },
    #[snafu(display("draining the frag cache left {missing} pages allocated"))]
    NotReturned { missing: usize },
    #[snafu(display("alloc_skb returned NULL"))]
    SkbAlloc,
    #[snafu(display("an alloc_skb/kfree_skb cycle left {first} free pages, the next {second}"))]
    SkbLeak { first: usize, second: usize },
}

fn free_pages() -> usize {
    ferrite_lkpi::mem::free_page_count().unwrap_or(0)
}

/// The flat page model through stock Linux: two page-sized frags from one frag cache share a compound
/// block, the second in a tail page; freeing both drops references on the head without freeing it,
/// since the cache's bias holds it, and draining the cache returns all of it. Then two `alloc_skb` and
/// `kfree_skb` cycles, the second leaving the free page count where the first did.
fn page_model() -> Result<(), PageModelError> {
    let mut cache = PageFragCache([0; 2]);
    let before = free_pages();
    // SAFETY: called on a task after the kernel start-up; the cache is drained before it goes.
    let (first, second) = unsafe {
        let size = PAGE_SIZE as c_uint;
        (
            __page_frag_alloc_align(&raw mut cache, size, GFP_KERNEL, !0),
            __page_frag_alloc_align(&raw mut cache, size, GFP_KERNEL, !0),
        )
    };
    if first.is_null() || second.is_null() {
        return Err(PageModelError::FragAlloc);
    }
    let (a, b) = (first.addr(), second.addr());
    if b != a + PAGE_SIZE || !a.is_multiple_of(PAGE_SIZE * FRAG_BLOCK_PAGES) {
        return Err(PageModelError::FragLayout { first: a, second: b });
    }
    let taken = before - free_pages();
    if taken != FRAG_BLOCK_PAGES {
        return Err(PageModelError::Taken { pages: taken });
    }
    // SAFETY: both frags came from the cache above and are freed once.
    unsafe {
        page_frag_free(second);
        page_frag_free(first);
    }
    if free_pages() != before - FRAG_BLOCK_PAGES {
        return Err(PageModelError::FreedEarly { missing: before - free_pages() });
    }
    // SAFETY: the cache was filled above, and is not used again.
    unsafe { page_frag_cache_drain(&raw mut cache) };
    if free_pages() != before {
        return Err(PageModelError::NotReturned { missing: before - free_pages() });
    }

    let mut counts = [0; 2];
    for count in &mut counts {
        // SAFETY: on a task after skb_init; the skb is dropped at once and never shared.
        unsafe {
            let skb = __alloc_skb(1500, GFP_KERNEL, 0, NUMA_NO_NODE);
            if skb.is_null() {
                return Err(PageModelError::SkbAlloc);
            }
            sk_skb_reason_drop(core::ptr::null_mut(), skb, SKB_DROP_NOT_SPECIFIED);
        }
        *count = free_pages();
    }
    if counts[0] != counts[1] {
        return Err(PageModelError::SkbLeak { first: counts[0], second: counts[1] });
    }
    Ok(())
}
