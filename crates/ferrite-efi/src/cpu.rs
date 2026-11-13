//! Which vector extensions the CPU has and which state the firmware left enabled for them.
//!
//! AVX code needs CR4.OSXSAVE set and XCR0 bits 1 and 2 (SSE and AVX state); AVX-512 also needs bits 5
//! to 7. The apps report it at start-up and change neither register.

use core::arch::x86_64::{__cpuid, __cpuid_count, _xgetbv};
use core::fmt;

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
