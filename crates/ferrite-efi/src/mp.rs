//! The other cores, through the firmware's MP Services protocol, while boot services run.
//!
//! An AP may call no EFI service, allocate, log or take a lock the BSP might hold. It runs on the
//! firmware's AP stack (EDK2 `PcdCpuApStackSize`, 32 KiB by default and in OVMF), and if it is still
//! busy when its dispatch's timeout expires, the firmware's status check (every 100 ms by default,
//! from a timer event on the BSP) sends it INIT-SIPI-SIPI: whatever it was doing is abandoned, CR4
//! comes back from the copy the firmware saved after its previous task and XCR0 from INIT, and it
//! waits in the firmware's idle loop again. [`ApTask`] is the contract for code that runs there;
//! [`Mp::scope`] dispatches it without blocking and waits for every AP before the task can go away.

use alloc::vec::Vec;
use core::arch::x86_64::{__cpuid, __cpuid_count};
use core::cell::RefCell;
use core::ffi::c_void;
use core::fmt;
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use core::time::Duration;

use snafu::{ResultExt, Snafu};
use uefi::Event;
use uefi::boot::{self, EventType, OpenProtocolAttributes, OpenProtocolParams, ScopedProtocol, Tpl};
use uefi::proto::pi::mp::MpServices;

use crate::cpu::{FpuControl, VectorSupport};

/// The firmware's index of a logical processor, below its processor count. Only [`Mp`] makes one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProcessorNumber(usize);

impl ProcessorNumber {
    #[must_use]
    pub const fn get(self) -> usize {
        self.0
    }
}

impl fmt::Display for ProcessorNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cpu{}", self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Location {
    pub package: u32,
    pub core: u32,
    pub thread: u32,
}

/// What CPUID leaf 0x1A reports on the core itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoreKind {
    /// Not a hybrid part: every core is the same.
    Uniform,
    Performance,
    Efficient,
    Unknown(u8),
}

impl CoreKind {
    const fn from_leaf_1a(eax: u32) -> Self {
        match (eax >> 24) as u8 {
            0x20 => Self::Efficient,
            0x40 => Self::Performance,
            raw => Self::Unknown(raw),
        }
    }
}

impl fmt::Display for CoreKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Uniform => f.write_str("uniform"),
            Self::Performance => f.write_str("P"),
            Self::Efficient => f.write_str("E"),
            Self::Unknown(raw) => write!(f, "type {raw:#x}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Processor {
    pub number: ProcessorNumber,
    /// The firmware's processor ID, the APIC ID on EDK2.
    pub id: u64,
    pub bsp: bool,
    pub enabled: bool,
    pub healthy: bool,
    pub location: Location,
    /// `None` when the core did not answer the identify dispatch.
    pub kind: Option<CoreKind>,
}

impl fmt::Display for Processor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Location { package, core, thread } = self.location;
        write!(f, "{} id {:#x} pkg {package} core {core} thread {thread}", self.number, self.id)?;
        match self.kind {
            Some(kind) => write!(f, " {kind}")?,
            None => f.write_str(" unidentified")?,
        }
        if self.bsp {
            f.write_str(" bsp")?;
        }
        if !self.enabled {
            f.write_str(" disabled")?;
        }
        if !self.healthy {
            f.write_str(" unhealthy")?;
        }
        Ok(())
    }
}

#[derive(Debug, Snafu)]
pub enum MpError {
    #[snafu(display("the firmware has no MP Services protocol: {source}"))]
    NoProtocol { source: uefi::Error },
    #[snafu(display("counting the processors failed: {source}"))]
    Count { source: uefi::Error },
    #[snafu(display("reading processor {number}'s information failed: {source}"))]
    Info { number: usize, source: uefi::Error },
    #[snafu(display("creating the completion event for {number} failed: {source}"))]
    CreateEvent { number: ProcessorNumber, source: uefi::Error },
    #[snafu(display("starting {number} failed: {source}"))]
    Start { number: ProcessorNumber, source: uefi::Error },
    #[snafu(display("{number} is the BSP or disabled, so it cannot be started"))]
    NotAnAp { number: ProcessorNumber },
}

/// Bytes below the AP's entry stack pointer painted to measure its stack use: a quarter of EDK2's
/// default AP stack, so the paint never reaches the next AP's stack below.
pub const STACK_PROBE: usize = 8 << 10;
const STACK_PAINT: u64 = 0x5354_4143_4b50_4149;

/// How deep an AP task's stack went below the dispatch entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StackUse {
    bytes: usize,
}

impl StackUse {
    #[must_use]
    pub const fn bytes(self) -> usize {
        self.bytes
    }

    /// Whether the task used the whole probe, so the real depth is unknown.
    #[must_use]
    pub const fn exhausted(self) -> bool {
        self.bytes >= STACK_PROBE
    }
}

impl fmt::Display for StackUse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.exhausted() { write!(f, ">= {STACK_PROBE} B") } else { write!(f, "{} B", self.bytes) }
    }
}

/// Work that runs on an AP.
///
/// # Safety
/// `run` and `finish` must call no EFI service, allocate nothing, log nothing, take no lock the BSP
/// may hold and stay within [`STACK_PROBE`] bytes of stack. A panic there halts the core with
/// interrupts off ([`halt_ap`]) until the dispatch's timeout resets it.
pub unsafe trait ApTask: Sync {
    fn run(&self);

    /// The last call on the AP, after `run`, with the stack depth `run` reached.
    fn finish(&self, stack: StackUse);
}

extern "efiapi" fn trampoline<T: ApTask>(arg: *mut c_void) {
    let floor: usize;
    // SAFETY: the STACK_PROBE bytes below RSP are free stack on the firmware's AP stack, which is far
    // larger; `rep stosq` with DF clear (the ABI's guarantee) writes exactly that range and no stack.
    unsafe {
        core::arch::asm!(
            "mov {floor}, rsp",
            "sub {floor}, {probe}",
            "mov rdi, {floor}",
            "rep stosq",
            floor = out(reg) floor,
            probe = const STACK_PROBE,
            out("rdi") _,
            inout("rcx") STACK_PROBE / 8 => _,
            in("rax") STACK_PAINT,
            options(nostack, preserves_flags),
        );
    }
    // SAFETY: `Scope::start` passed a `&T` the scope keeps alive until this dispatch has ended.
    let task = unsafe { &*arg.cast_const().cast::<T>() };
    task.run();
    let left: usize;
    let clean: u8;
    // SAFETY: reads back the painted range, which is still this AP's stack; `repe scasq` only reads.
    unsafe {
        core::arch::asm!(
            "repe scasq",
            "setz {clean}",
            clean = out(reg_byte) clean,
            inout("rdi") floor => _,
            inout("rcx") STACK_PROBE / 8 => left,
            in("rax") STACK_PAINT,
            options(nostack, readonly),
        );
    }
    let words = STACK_PROBE / 8;
    let untouched = if clean != 0 { words } else { words - left - 1 };
    task.finish(StackUse { bytes: (words - untouched) * 8 });
}

const NO_BSP: u32 = u32::MAX;
static BSP_APIC: AtomicU32 = AtomicU32::new(NO_BSP);
static AP_PANICS: AtomicU32 = AtomicU32::new(0);

/// The running core's APIC ID from CPUID: the x2APIC ID from leaf 0xB, else the initial xAPIC ID.
#[must_use]
pub fn apic_id() -> u32 {
    if __cpuid(0).eax >= 0xb {
        let leaf = __cpuid_count(0xb, 0);
        if leaf.ebx != 0 {
            return leaf.edx;
        }
    }
    __cpuid(1).ebx >> 24
}

/// Whether this core is an AP, once [`Mp::locate`] has recorded the BSP; false before.
#[must_use]
pub fn on_ap() -> bool {
    let bsp = BSP_APIC.load(Ordering::Relaxed);
    bsp != NO_BSP && apic_id() != bsp
}

/// The panic handler's path on an AP: count the panic and halt with interrupts off, touching nothing
/// else, until the firmware's dispatch timeout resets the core.
pub fn halt_ap() -> ! {
    AP_PANICS.fetch_add(1, Ordering::Relaxed);
    loop {
        // SAFETY: CPL 0; stops this core only. INIT, which the firmware's timeout sends, still wakes it.
        unsafe { core::arch::asm!("cli", "hlt", options(nomem, nostack)) };
    }
}

/// APs that have panicked since start-up.
#[must_use]
pub fn ap_panics() -> u32 {
    AP_PANICS.load(Ordering::Relaxed)
}

/// MP Services, and the processors it reported at start-up.
pub struct Mp {
    proto: ManuallyDrop<ScopedProtocol<MpServices>>,
    processors: Vec<Processor>,
}

impl fmt::Debug for Mp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mp").field("processors", &self.processors).finish_non_exhaustive()
    }
}

/// How long the identify dispatch may take on one AP; it runs a few CPUID instructions.
const IDENTIFY_TIMEOUT: Duration = Duration::from_secs(1);
/// The longest timeout handed to the firmware, which counts microseconds in a `usize`.
const MAX_TIMEOUT: Duration = Duration::from_secs(3600);

impl Mp {
    /// Open the protocol (shared, never closed, as with the RNG), list the processors and run CPUID
    /// leaf 0x1A on each of them.
    pub fn locate() -> Result<Self, MpError> {
        let handle = boot::get_handle_for_protocol::<MpServices>().context(NoProtocolSnafu)?;
        let params = OpenProtocolParams { handle, agent: boot::image_handle(), controller: None };
        // SAFETY: a shared open; MP Services is only called from the BSP, never re-entrantly.
        let proto = unsafe { boot::open_protocol::<MpServices>(params, OpenProtocolAttributes::GetProtocol) }
            .context(NoProtocolSnafu)?;
        let count = proto.get_number_of_processors().context(CountSnafu)?;
        let processors = (0..count.total)
            .map(|number| {
                let info = proto.get_processor_info(number).context(InfoSnafu { number })?;
                Ok(Processor {
                    number: ProcessorNumber(number),
                    id: info.processor_id,
                    bsp: info.is_bsp(),
                    enabled: info.is_enabled(),
                    healthy: info.is_healthy(),
                    location: Location {
                        package: info.location.package,
                        core: info.location.core,
                        thread: info.location.thread,
                    },
                    kind: None,
                })
            })
            .collect::<Result<Vec<_>, MpError>>()?;
        BSP_APIC.store(apic_id(), Ordering::Relaxed);
        let mut mp = Self { proto: ManuallyDrop::new(proto), processors };
        mp.identify();
        Ok(mp)
    }

    #[must_use]
    pub fn processors(&self) -> &[Processor] {
        &self.processors
    }

    /// Enabled processors other than the BSP: the ones [`Scope::start`] accepts.
    pub fn aps(&self) -> impl Iterator<Item = &Processor> {
        self.processors.iter().filter(|p| p.enabled && !p.bsp)
    }

    fn identify(&mut self) {
        let answers: Vec<Identify> = self.processors.iter().map(|_| Identify::default()).collect();
        let aps: Vec<ProcessorNumber> = self.aps().map(|p| p.number).collect();
        self.scope(|s| {
            for &number in &aps {
                if let Err(e) = s.start(number, &answers[number.0], IDENTIFY_TIMEOUT) {
                    log::warn!("mp: identify: {e}");
                }
            }
        });
        for (p, answer) in self.processors.iter_mut().zip(&answers) {
            if p.bsp {
                answer.run();
            }
            if answer.done.load(Ordering::Acquire) {
                let leaf = answer.leaf_1a.load(Ordering::Relaxed);
                p.kind = Some(if answer.hybrid.load(Ordering::Relaxed) {
                    CoreKind::from_leaf_1a(leaf)
                } else {
                    CoreKind::Uniform
                });
            }
        }
    }

    /// CR4 and XCR0 as each AP has them now, to check that AP work put them back; `None` for an AP
    /// that did not answer.
    pub fn ap_control_registers(&self) -> Vec<(ProcessorNumber, Option<ControlRegisters>)> {
        let aps: Vec<ProcessorNumber> = self.aps().map(|p| p.number).collect();
        let answers: Vec<ReadControl> = aps.iter().map(|_| ReadControl::default()).collect();
        self.scope(|s| {
            for (&number, answer) in aps.iter().zip(&answers) {
                if let Err(e) = s.start(number, answer, IDENTIFY_TIMEOUT) {
                    log::warn!("mp: reading control registers: {e}");
                }
            }
        });
        aps.into_iter().zip(&answers).map(|(number, a)| (number, a.get())).collect()
    }

    /// Run `f` with a [`Scope`] to start AP tasks in, then wait for every task started there to end.
    ///
    /// The wait spins on the completion events, which the firmware signals from its periodic status
    /// check, so it ends up to one check interval after the last AP returns.
    pub fn scope<'env, F, R>(&'env self, f: F) -> R
    where
        F: for<'scope> FnOnce(&'scope Scope<'scope, 'env>) -> R,
    {
        let scope = Scope { mp: self, started: RefCell::new(Vec::new()), _borrow: PhantomData, _env: PhantomData };
        let result = f(&scope);
        while scope.running() > 0 {
            core::hint::spin_loop();
        }
        for started in scope.started.take() {
            if let Err(e) = boot::close_event(started.event) {
                log::debug!("mp: closing {}'s completion event failed: {e}", started.number);
            }
        }
        result
    }
}

struct Started {
    number: ProcessorNumber,
    event: Event,
    ended: bool,
    check_failed: bool,
}

/// Where AP tasks are started; every one has ended before [`Mp::scope`] returns.
pub struct Scope<'scope, 'env: 'scope> {
    mp: &'env Mp,
    started: RefCell<Vec<Started>>,
    _borrow: PhantomData<&'scope mut &'scope ()>,
    _env: PhantomData<&'env mut &'env ()>,
}

impl<'scope> Scope<'scope, '_> {
    /// Start `task` on AP `number` without waiting for it. The firmware resets the AP if the task
    /// is still running after `timeout` (capped at an hour).
    pub fn start<T: ApTask>(
        &'scope self,
        number: ProcessorNumber,
        task: &'scope T,
        timeout: Duration,
    ) -> Result<(), MpError> {
        if !self.mp.aps().any(|p| p.number == number) {
            return Err(MpError::NotAnAp { number });
        }
        // SAFETY: no notify function, so nothing runs when the firmware signals it.
        let event = unsafe { boot::create_event(EventType::empty(), Tpl::APPLICATION, None, None) }
            .context(CreateEventSnafu { number })?;
        let timeout = timeout.clamp(Duration::from_micros(1), MAX_TIMEOUT);
        let arg = core::ptr::from_ref(task).cast_mut().cast::<c_void>();
        // SAFETY: the clone is the firmware's; ours stays open until the scope has seen it signalled.
        let firmware_event = unsafe { event.unsafe_clone() };
        if let Err(source) =
            self.mp.proto.startup_this_ap(number.0, trampoline::<T>, arg, Some(firmware_event), Some(timeout))
        {
            if let Err(e) = boot::close_event(event) {
                log::debug!("mp: closing {number}'s completion event failed: {e}");
            }
            return Err(MpError::Start { number, source });
        }
        self.started.borrow_mut().push(Started { number, event, ended: false, check_failed: false });
        Ok(())
    }

    /// Tasks started here whose dispatch the firmware has not yet reported ended (done or timed out).
    pub fn running(&self) -> usize {
        let mut started = self.started.borrow_mut();
        for s in started.iter_mut().filter(|s| !s.ended) {
            match boot::check_event(&s.event) {
                Ok(signalled) => s.ended = signalled,
                Err(e) if !s.check_failed => {
                    s.check_failed = true;
                    log::warn!("mp: checking {}'s completion event failed, still waiting: {e}", s.number);
                },
                Err(_) => {},
            }
        }
        started.iter().filter(|s| !s.ended).count()
    }
}

#[derive(Default)]
struct Identify {
    hybrid: AtomicBool,
    leaf_1a: AtomicU32,
    done: AtomicBool,
}

// SAFETY: CPUID and stores to atomics.
unsafe impl ApTask for Identify {
    fn run(&self) {
        let max = __cpuid(0).eax;
        let hybrid = max >= 7 && __cpuid_count(7, 0).edx & (1 << 15) != 0;
        if hybrid && max >= 0x1a {
            self.leaf_1a.store(__cpuid_count(0x1a, 0).eax, Ordering::Relaxed);
        }
        self.hybrid.store(hybrid, Ordering::Relaxed);
        self.done.store(true, Ordering::Release);
    }

    fn finish(&self, _stack: StackUse) {}
}

/// CR4 and XCR0 on one core; `xcr0` is `None` while CR4.OSXSAVE is clear.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlRegisters {
    pub cr4: u64,
    pub xcr0: Option<u64>,
}

impl ControlRegisters {
    /// The calling core's.
    #[must_use]
    pub fn read() -> Self {
        Self { cr4: FpuControl::read().cr4, xcr0: VectorSupport::probe().xcr0 }
    }
}

impl fmt::Display for ControlRegisters {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.xcr0 {
            Some(xcr0) => write!(f, "cr4 {:#x} xcr0 {xcr0:#x}", self.cr4),
            None => write!(f, "cr4 {:#x} xcr0 -", self.cr4),
        }
    }
}

#[derive(Default)]
struct ReadControl {
    cr4: AtomicU64,
    xcr0: AtomicU64,
    has_xcr0: AtomicBool,
    done: AtomicBool,
}

impl ReadControl {
    fn get(&self) -> Option<ControlRegisters> {
        self.done.load(Ordering::Acquire).then(|| ControlRegisters {
            cr4: self.cr4.load(Ordering::Relaxed),
            xcr0: self.has_xcr0.load(Ordering::Relaxed).then(|| self.xcr0.load(Ordering::Relaxed)),
        })
    }
}

// SAFETY: reads control registers and CPUID, and stores to atomics.
unsafe impl ApTask for ReadControl {
    fn run(&self) {
        let regs = ControlRegisters::read();
        self.cr4.store(regs.cr4, Ordering::Relaxed);
        if let Some(xcr0) = regs.xcr0 {
            self.xcr0.store(xcr0, Ordering::Relaxed);
            self.has_xcr0.store(true, Ordering::Relaxed);
        }
        self.done.store(true, Ordering::Release);
    }

    fn finish(&self, _stack: StackUse) {}
}
