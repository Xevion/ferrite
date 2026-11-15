//! The privileged x86 instructions every module shares: control registers, the interrupt flag,
//! CPUID and MSR access.
//!
//! [`ProbingMsrs`] reads and writes MSRs that may not exist. For the length of one access it loads a
//! copy of the firmware's IDT whose general-protection gate resumes past the faulting `rdmsr` or
//! `wrmsr` and flags the fault, the way Linux's `rdmsr_safe` uses an exception fixup. Interrupts
//! are off for that window, so nothing else can reach the copied table. Use it on the boot
//! processor; [`RawMsrs`] is for APs touching registers the boot processor already found.

use core::arch::x86_64::__cpuid_count;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

use ferrite_power::cpuid::{CpuidSource, Leaf};
use ferrite_power::msr::{Fault, MsrIo};

const RFLAGS_IF: u64 = 1 << 9;
/// The general-protection exception's vector.
const GP_VECTOR: usize = 13;
/// A present, ring-0, 64-bit interrupt gate.
const INTERRUPT_GATE: u64 = 0x8e;
/// 256 gates of 16 bytes.
const IDT_WORDS: usize = 512;

pub(crate) fn read_cr0() -> u64 {
    let v: u64;
    // SAFETY: CPL 0.
    unsafe { core::arch::asm!("mov {}, cr0", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

/// # Safety
/// `v` is a valid CR0 for the running system.
pub(crate) unsafe fn write_cr0(v: u64) {
    // SAFETY: the caller's.
    unsafe { core::arch::asm!("mov cr0, {}", in(reg) v, options(nostack, preserves_flags)) };
}

pub(crate) fn read_cr3() -> u64 {
    let v: u64;
    // SAFETY: CPL 0.
    unsafe { core::arch::asm!("mov {}, cr3", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

pub(crate) fn read_cr4() -> u64 {
    let v: u64;
    // SAFETY: CPL 0.
    unsafe { core::arch::asm!("mov {}, cr4", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

/// # Safety
/// `v` is a valid CR4 for the running system.
pub(crate) unsafe fn write_cr4(v: u64) {
    // SAFETY: the caller's.
    unsafe { core::arch::asm!("mov cr4, {}", in(reg) v, options(nostack, preserves_flags)) };
}

/// Run `f` with interrupts off, then turn them back on if they were on.
pub(crate) fn without_interrupts<R>(f: impl FnOnce() -> R) -> R {
    let flags: u64;
    // SAFETY: CPL 0; reads RFLAGS through the stack and clears IF.
    unsafe { core::arch::asm!("pushfq", "pop {}", "cli", out(reg) flags, options(nomem)) };
    let result = f();
    if flags & RFLAGS_IF != 0 {
        // SAFETY: interrupts were on before.
        unsafe { core::arch::asm!("sti", options(nomem, nostack)) };
    }
    result
}

/// # Safety
/// The MSR exists.
pub(crate) unsafe fn read_msr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: the caller's; CPL 0.
    unsafe { core::arch::asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nomem, nostack)) };
    u64::from(hi) << 32 | u64::from(lo)
}

/// # Safety
/// The MSR exists and `v` is valid for it.
pub(crate) unsafe fn write_msr(msr: u32, v: u64) {
    // SAFETY: the caller's; CPL 0.
    unsafe {
        core::arch::asm!("wrmsr", in("ecx") msr, in("eax") v as u32, in("edx") (v >> 32) as u32, options(nostack));
    }
}

/// CPUID on the calling core.
#[derive(Clone, Copy, Debug, Default)]
pub struct Cpuid;

impl CpuidSource for Cpuid {
    fn cpuid(&self, leaf: u32, subleaf: u32) -> Leaf {
        let r = __cpuid_count(leaf, subleaf);
        Leaf { eax: r.eax, ebx: r.ebx, ecx: r.ecx, edx: r.edx }
    }
}

/// MSR access with no fault handling, for registers known to exist on this core.
#[derive(Debug)]
pub struct RawMsrs(());

impl RawMsrs {
    /// # Safety
    /// Every register accessed through it exists on the calling core and takes the values written,
    /// as discovery on the boot processor found for every core of the package.
    #[must_use]
    pub const unsafe fn new() -> Self {
        Self(())
    }
}

impl MsrIo for RawMsrs {
    fn read_raw(&mut self, address: u32) -> Result<u64, Fault> {
        // SAFETY: `new`'s contract.
        Ok(unsafe { read_msr(address) })
    }

    fn write_raw(&mut self, address: u32, value: u64) -> Result<(), Fault> {
        // SAFETY: `new`'s contract.
        unsafe { write_msr(address, value) };
        Ok(())
    }
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct Idtr {
    limit: u16,
    base: u64,
}

#[repr(C, align(16))]
struct Table(UnsafeCell<[u64; IDT_WORDS]>);

// SAFETY: only the holder of `TABLE_HELD` touches it.
unsafe impl Sync for Table {}

static TABLE: Table = Table(UnsafeCell::new([0; IDT_WORDS]));
static TABLE_HELD: AtomicBool = AtomicBool::new(false);

/// Resumes after the 2-byte `rdmsr` or `wrmsr` that faulted, with R8 set to say so.
#[unsafe(naked)]
extern "C" fn resume_past_fault() {
    core::arch::naked_asm!("add rsp, 8", "add qword ptr [rsp], 2", "mov r8d, 1", "iretq");
}

/// MSR access that turns a missing register's fault into [`Fault`]. One exists at a time.
pub struct ProbingMsrs {
    /// The firmware IDT the copy was made from.
    firmware: u64,
}

impl ProbingMsrs {
    /// `None` while another one exists.
    #[must_use]
    pub fn new() -> Option<Self> {
        TABLE_HELD.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).ok()?;
        Some(Self { firmware: 0 })
    }

    /// Run `access` with the copied IDT loaded and interrupts off; true when it faulted.
    fn guarded(&mut self, access: impl FnOnce() -> bool) -> bool {
        without_interrupts(|| {
            let mut old = Idtr { limit: 0, base: 0 };
            // SAFETY: stores the IDTR into `old`.
            unsafe { core::arch::asm!("sidt [{}]", in(reg) &raw mut old, options(nostack, preserves_flags)) };
            if old.base != self.firmware {
                self.copy(old);
            }
            let ours = Idtr { limit: (IDT_WORDS * 8 - 1) as u16, base: TABLE.0.get() as u64 };
            // SAFETY: the copy holds every firmware gate, with only #GP replaced, and interrupts are off.
            unsafe {
                core::arch::asm!("lidt [{}]", in(reg) &raw const ours, options(readonly, nostack, preserves_flags));
            };
            let faulted = access();
            // SAFETY: the firmware's own IDTR.
            unsafe {
                core::arch::asm!("lidt [{}]", in(reg) &raw const old, options(readonly, nostack, preserves_flags));
            };
            faulted
        })
    }

    /// Copy the firmware's gates and point #GP at [`resume_past_fault`].
    fn copy(&mut self, firmware: Idtr) {
        let words = (usize::from(firmware.limit) + 1).min(IDT_WORDS * 8) / 8;
        // SAFETY: `TABLE_HELD` is ours; the firmware IDT is `limit + 1` readable bytes.
        let table = unsafe { &mut *TABLE.0.get() };
        // SAFETY: as above.
        let source = unsafe { core::slice::from_raw_parts(firmware.base as *const u64, words) };
        table.fill(0);
        table[..words].copy_from_slice(source);
        let selector: u16;
        // SAFETY: reads CS.
        unsafe { core::arch::asm!("mov {:x}, cs", out(reg) selector, options(nomem, nostack, preserves_flags)) };
        let handler = resume_past_fault as *const () as u64;
        table[2 * GP_VECTOR] =
            handler & 0xffff | u64::from(selector) << 16 | INTERRUPT_GATE << 40 | (handler >> 16 & 0xffff) << 48;
        table[2 * GP_VECTOR + 1] = handler >> 32;
        self.firmware = firmware.base;
    }
}

impl Drop for ProbingMsrs {
    fn drop(&mut self) {
        TABLE_HELD.store(false, Ordering::Release);
    }
}

impl MsrIo for ProbingMsrs {
    fn read_raw(&mut self, address: u32) -> Result<u64, Fault> {
        let (mut lo, mut hi) = (0_u32, 0_u32);
        let faulted = self.guarded(|| {
            let flag: u32;
            // SAFETY: a fault resumes after `rdmsr` with R8 = 1 through the loaded IDT. Not `nostack`:
            // the exception frame lands on this stack.
            unsafe {
                core::arch::asm!("xor r8d, r8d", "rdmsr", in("ecx") address, out("eax") lo, out("edx") hi, out("r8") flag);
            }
            flag != 0
        });
        if faulted { Err(Fault { address }) } else { Ok(u64::from(hi) << 32 | u64::from(lo)) }
    }

    fn write_raw(&mut self, address: u32, value: u64) -> Result<(), Fault> {
        let faulted = self.guarded(|| {
            let flag: u32;
            // SAFETY: as for the read.
            unsafe {
                core::arch::asm!(
                    "xor r8d, r8d", "wrmsr",
                    in("ecx") address, in("eax") value as u32, in("edx") (value >> 32) as u32, out("r8") flag,
                );
            }
            flag != 0
        });
        if faulted { Err(Fault { address }) } else { Ok(()) }
    }
}
