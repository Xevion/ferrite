//! Which vector extensions the CPU has and which state the firmware left enabled for them.
//!
//! AVX code needs CR4.OSXSAVE set and XCR0 bits 1 and 2 (SSE and AVX state); AVX-512 also needs bits 5
//! to 7. The apps report it at start-up; [`VectorState::enable`] turns the state on for an app's
//! lifetime and puts both registers back when it ends.

use core::arch::x86_64::{__cpuid, __cpuid_count, _xgetbv, _xsetbv};
use core::fmt;

use snafu::Snafu;

use crate::x86::{read_cr4, without_interrupts, write_cr4};

/// XCR0 state-component bits that AVX needs: SSE (1) and AVX (2).
const XCR0_AVX: u64 = 0b110;
/// XCR0 bits AVX-512 adds: opmask, ZMM upper halves, ZMM16-31.
const XCR0_AVX512: u64 = 0b1110_0000;

/// What CPUID reports, and XCR0 when the OS-enable bit lets it be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VectorSupport {
    pub sse2: bool,
    pub avx2: bool,
    pub avx512f: bool,
    /// `None` when CR4.OSXSAVE is clear, which makes `xgetbv` fault.
    pub xcr0: Option<u64>,
}

impl VectorSupport {
    pub fn probe() -> Self {
        let leaf1 = __cpuid(1);
        let max_leaf = __cpuid(0).eax;
        let leaf7 = if max_leaf >= 7 { __cpuid_count(7, 0).ebx } else { 0 };
        let osxsave = leaf1.ecx & (1 << 27) != 0;
        Self {
            sse2: leaf1.edx & (1 << 26) != 0,
            avx2: leaf7 & (1 << 5) != 0,
            avx512f: leaf7 & (1 << 16) != 0,
            // SAFETY: OSXSAVE says CR4.OSXSAVE is set, so `xgetbv` with ECX = 0 is defined.
            xcr0: osxsave.then(|| unsafe { read_xcr0() }),
        }
    }

    /// Whether AVX2 code can run now, without touching CR4 or XCR0.
    pub fn avx2_usable(&self) -> bool {
        self.avx2 && self.xcr0.is_some_and(|x| x & XCR0_AVX == XCR0_AVX)
    }

    /// Whether AVX-512F code can run now.
    pub fn avx512f_usable(&self) -> bool {
        let want = XCR0_AVX | XCR0_AVX512;
        self.avx512f && self.xcr0.is_some_and(|x| x & want == want)
    }
}

const CR0_EM: u64 = 1 << 2;
const CR0_TS: u64 = 1 << 3;
const CR4_OSFXSR: u64 = 1 << 9;
const CR4_OSXSAVE: u64 = 1 << 18;

/// CR0, CR4 and MXCSR as the firmware left them, read with inline asm alone so no SSE runs first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FpuControl {
    pub cr0: u64,
    pub cr4: u64,
    /// `None` when SSE is not enabled, which makes `stmxcsr` fault.
    pub mxcsr: Option<u32>,
}

impl FpuControl {
    /// Read the registers. Runs at CPL 0, as a UEFI app does.
    #[expect(clippy::inline_always, reason = "inlined into the entry point, before any call that may use SSE")]
    #[inline(always)]
    pub fn read() -> Self {
        let (cr0, cr4): (u64, u64);
        // SAFETY: reading control registers at CPL 0 has no side effects.
        unsafe {
            core::arch::asm!("mov {}, cr0", "mov {}, cr4", out(reg) cr0, out(reg) cr4,
                options(nomem, nostack, preserves_flags));
        }
        let ready = cr0 & (CR0_EM | CR0_TS) == 0 && cr4 & CR4_OSFXSR != 0;
        let mxcsr = ready.then(|| {
            let mut value = 0_u32;
            // SAFETY: SSE is enabled, so stmxcsr only stores four bytes to `value`.
            unsafe {
                core::arch::asm!("stmxcsr [{}]", in(reg) &raw mut value, options(nostack, preserves_flags));
            }
            value
        });
        Self { cr0, cr4, mxcsr }
    }

    /// Whether SSE instructions run: CR0.EM and CR0.TS clear, CR4.OSFXSR set.
    pub const fn sse_ready(&self) -> bool {
        self.mxcsr.is_some()
    }
}

impl fmt::Display for FpuControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bit = |reg: u64, mask: u64| u8::from(reg & mask != 0);
        write!(
            f,
            "cr0={:#x} (em={} ts={}) cr4={:#x} (osfxsr={} osxsave={}) ",
            self.cr0,
            bit(self.cr0, CR0_EM),
            bit(self.cr0, CR0_TS),
            self.cr4,
            bit(self.cr4, CR4_OSFXSR),
            bit(self.cr4, CR4_OSXSAVE)
        )?;
        match self.mxcsr {
            Some(mxcsr) => write!(f, "mxcsr={mxcsr:#x}"),
            None => f.write_str("mxcsr unreadable: SSE is not enabled"),
        }
    }
}

/// # Safety
/// CR4.OSXSAVE must be set.
#[target_feature(enable = "xsave")]
unsafe fn read_xcr0() -> u64 {
    // SAFETY: the caller checked OSXSAVE.
    unsafe { _xgetbv(0) }
}

impl fmt::Display for VectorSupport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cpuid sse2={} avx2={} avx512f={}, ", self.sse2, self.avx2, self.avx512f)?;
        match self.xcr0 {
            Some(xcr0) => write!(f, "xcr0={xcr0:#x}")?,
            None => f.write_str("xcr0 unreadable (CR4.OSXSAVE clear)")?,
        }
        write!(f, "; usable now: avx2={} avx512f={}", self.avx2_usable(), self.avx512f_usable())
    }
}

/// XCR0 bit for x87 state, which must always be set.
const XCR0_X87: u64 = 1;

#[derive(Debug, Snafu, PartialEq, Eq)]
pub enum XsaveError {
    #[snafu(display("CPUID reports no XSAVE, so AVX state cannot be enabled"))]
    NoXsave,
}

/// AVX (and AVX-512) state turned on by this app; CR4, XCR0 and MXCSR go back to the firmware's values on drop.
///
/// The loader and later apps run after us, so nothing we change may outlive the app. A panic resets
/// the machine instead of returning, so it needs no restore.
#[derive(Debug)]
#[must_use = "dropping it restores CR4 and XCR0 at once"]
pub struct VectorState {
    saved_cr4: u64,
    saved_xcr0: u64,
    xcr0: u64,
    /// The app's float maths leave exception flags (precision) set, which the next image would inherit.
    saved_mxcsr: u32,
}

impl VectorState {
    /// Set CR4.OSXSAVE, then add to XCR0 what CPUID leaf 0xD reports supported among x87, SSE and AVX,
    /// and the opmask and ZMM components when AVX-512F is present and all three are supported.
    pub fn enable() -> Result<Self, XsaveError> {
        let leaf1 = __cpuid(1);
        let max_leaf = __cpuid(0).eax;
        if leaf1.ecx & (1 << 26) == 0 || max_leaf < 0xd {
            return Err(XsaveError::NoXsave);
        }
        let avx = leaf1.ecx & (1 << 28) != 0;
        let avx512f = max_leaf >= 7 && __cpuid_count(7, 0).ebx & (1 << 16) != 0;
        let supported = u64::from(__cpuid_count(0xd, 0).eax);
        let sse_avx = if avx { XCR0_AVX } else { XCR0_AVX & !0b100 };
        let mut want = XCR0_X87 | (supported & sse_avx);
        if avx && avx512f && supported & XCR0_AVX512 == XCR0_AVX512 {
            want |= XCR0_AVX512;
        }
        let saved_cr4 = read_cr4();
        if saved_cr4 & CR4_OSXSAVE == 0 {
            // SAFETY: CPUID reports XSAVE, so the bit is defined; it only enables XGETBV/XSETBV and XSAVE.
            unsafe { write_cr4(saved_cr4 | CR4_OSXSAVE) };
        }
        // SAFETY: CR4.OSXSAVE is set now. XCR0 kept its value while the bit was clear, so this is the original.
        let saved_xcr0 = unsafe { read_xcr0() };
        let xcr0 = saved_xcr0 | want;
        if xcr0 != saved_xcr0 {
            // SAFETY: every bit is one CPUID leaf 0xD reports supported, x87 is set, and the AVX-512
            // components are only added with AVX, as XSETBV requires.
            unsafe { write_xcr0(xcr0) };
        }
        Ok(Self { saved_cr4, saved_xcr0, xcr0, saved_mxcsr: read_mxcsr() })
    }

    /// XCR0 as the firmware left it, and as it is now.
    pub const fn xcr0(&self) -> (u64, u64) {
        (self.saved_xcr0, self.xcr0)
    }

    /// CR4 as the firmware left it.
    pub const fn saved_cr4(&self) -> u64 {
        self.saved_cr4
    }

    /// Run `f` with interrupts off, then clear the YMM and ZMM upper halves and restore IF.
    ///
    /// EDK2's interrupt entry saves only the FXSAVE state, so wide-register work must not be
    /// interrupted. Keep `f` short (well under 1 ms) so the firmware's timer is not starved.
    pub fn wide<R>(&self, f: impl FnOnce() -> R) -> R {
        without_interrupts(|| {
            let result = f();
            if self.xcr0 & XCR0_AVX == XCR0_AVX {
                // SAFETY: AVX state is enabled; no compiled code in this crate keeps values in upper halves.
                unsafe { core::arch::asm!("vzeroupper", options(nomem, nostack, preserves_flags)) };
            }
            result
        })
    }
}

impl Drop for VectorState {
    fn drop(&mut self) {
        // SAFETY: the value was read from MXCSR, so it has no reserved bits set.
        unsafe { write_mxcsr(self.saved_mxcsr) };
        if self.xcr0 != self.saved_xcr0 {
            // SAFETY: CR4.OSXSAVE is still set and the saved value was valid XCR0 before.
            unsafe { write_xcr0(self.saved_xcr0) };
        }
        if self.saved_cr4 & CR4_OSXSAVE == 0 {
            // SAFETY: restores the firmware's CR4, differing only in OSXSAVE.
            unsafe { write_cr4(self.saved_cr4) };
        }
    }
}

impl fmt::Display for VectorState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cr4 {:#x} -> {:#x}, xcr0 {:#x} -> {:#x}",
            self.saved_cr4,
            self.saved_cr4 | CR4_OSXSAVE,
            self.saved_xcr0,
            self.xcr0
        )
    }
}

/// MXCSR; SSE runs, since this crate is compiled with it.
fn read_mxcsr() -> u32 {
    let mut value = 0_u32;
    // SAFETY: stmxcsr only stores four bytes to `value`.
    unsafe { core::arch::asm!("stmxcsr [{}]", in(reg) &raw mut value, options(nostack, preserves_flags)) };
    value
}

/// # Safety
/// `value` must have no reserved MXCSR bit set, or `ldmxcsr` faults.
unsafe fn write_mxcsr(value: u32) {
    // SAFETY: the caller's contract.
    unsafe { core::arch::asm!("ldmxcsr [{}]", in(reg) &raw const value, options(nostack, preserves_flags, readonly)) };
}

/// # Safety
/// CR4.OSXSAVE must be set and `value` a combination XSETBV accepts.
#[target_feature(enable = "xsave")]
unsafe fn write_xcr0(value: u64) {
    // SAFETY: the caller's contract.
    unsafe { _xsetbv(0, value) };
}
