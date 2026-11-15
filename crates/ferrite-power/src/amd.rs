//! AMD's clock registers: CPPC and hardware P-states.
//!
//! CPPC (Zen 2 and later, where CPUID `0x8000_0008` EBX bit 27 says the MSRs exist) takes a range of
//! abstract performance levels in `CPPC_REQ` once `CPPC_ENABLE` is set, which lasts until reset.
//! `CPPC_CAP1` reads before that. Its nominal level is the base clock, P0, so the `PStateDef`
//! registers give the scale. On parts with preferred cores, `CPPC_CAP1`'s highest level is a
//! per-core ranking rather than a clock, so the request's maximum is each core's own highest.
//!
//! Without CPPC the clock is a P-state index in `PStateCtl`: 0 is the fastest non-boost state, and
//! Core Performance Boost runs above P0 on its own unless `HWCR` disables it.

use arbitrary_int::u3;
use bitbybit::bitfield;

use crate::msr::{Msr, register};
use crate::units::Mhz;

pub const HWCR: Msr<Hwcr> = Msr::at(0xc001_0015);
pub const PSTATE_LIMIT: Msr<PStateLimit> = Msr::at(0xc001_0061);
pub const PSTATE_CTL: Msr<PStateCtl> = Msr::at(0xc001_0062);
pub const PSTATE_STATUS: Msr<PStateStatus> = Msr::at(0xc001_0063);
/// The first of the eight P-state definitions.
pub const PSTATE_DEF: Msr<PStateDef> = Msr::at(0xc001_0064);
pub const CPPC_CAP1: Msr<CppcCapabilities> = Msr::at(0xc001_02b0);
pub const CPPC_ENABLE: Msr<CppcEnable> = Msr::at(0xc001_02b1);
pub const CPPC_REQ: Msr<CppcRequest> = Msr::at(0xc001_02b3);

/// How many `PStateDef` registers there are.
pub const PSTATES: u8 = 8;

/// `HWCR`, the bit that matters for the clock.
#[bitfield(u64, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct Hwcr {
    /// Core Performance Boost is off.
    #[bit(25, rw)]
    cpb_disable: bool,
}

/// `PStateCurLim`.
#[bitfield(u64, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct PStateLimit {
    /// The fastest P-state software may ask for now.
    #[bits(0..=2, r)]
    current_limit: u3,
    /// The slowest P-state there is.
    #[bits(4..=6, r)]
    max_value: u3,
}

/// `PStateCtl`.
#[bitfield(u64, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct PStateCtl {
    #[bits(0..=2, rw)]
    command: u3,
}

/// `PStateStat`.
#[bitfield(u64, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct PStateStatus {
    #[bits(0..=2, r)]
    current: u3,
}

/// One `PStateDef`. The frequency fields differ by family, so [`PStateDef::frequency`] takes it.
#[bitfield(u64, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct PStateDef {
    /// Family 10h to 16h: frequency ID.
    #[bits(0..=5, r)]
    k10_fid: arbitrary_int::u6,
    /// Family 10h to 16h: divisor ID, a power of two.
    #[bits(6..=8, r)]
    k10_did: u3,
    /// Families 17h to 19h: frequency ID, in 25 MHz steps.
    #[bits(0..=7, r)]
    zen_fid: u8,
    /// Families 17h to 19h: divisor ID, in eighths.
    #[bits(8..=13, r)]
    zen_did: arbitrary_int::u6,
    /// Family 1Ah on: frequency ID, in 5 MHz steps.
    #[bits(0..=11, r)]
    zen5_fid: arbitrary_int::u12,
    #[bit(63, r)]
    enabled: bool,
}

impl PStateDef {
    /// The core clock this state runs at, the way `cpupower` computes it; `None` for a disabled
    /// state, a family before 10h, or a field that gives no clock.
    #[must_use]
    pub fn frequency(self, family: u16) -> Option<Mhz> {
        if !self.enabled() {
            return None;
        }
        let mhz = match family {
            0x1a.. => {
                let fid = u32::from(self.zen5_fid().value());
                (fid > 0xf).then_some(fid * 5)?
            },
            0x17..=0x19 => {
                let did = u32::from(self.zen_did().value());
                (did != 0).then(|| 200 * u32::from(self.zen_fid()) / did)?
            },
            0x12 => {
                let did = self.raw_value() as u32 & 0xf;
                (100 * (u32::from(self.k10_fid().value()) + 0x10)).checked_shr(did)?
            },
            0x10..=0x16 => {
                let base = if family == 0x11 { 0x8 } else { 0x10 };
                (100 * (u32::from(self.k10_fid().value()) + base)) >> self.k10_did().value()
            },
            _ => return None,
        };
        (mhz != 0).then_some(Mhz(mhz))
    }
}

/// `CPPC_CAP1`: the performance levels this core allows.
#[bitfield(u64, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct CppcCapabilities {
    #[bits(0..=7, r)]
    lowest: u8,
    /// The lowest level that still saves energy per unit of work.
    #[bits(8..=15, r)]
    lowest_nonlinear: u8,
    /// The base clock's level.
    #[bits(16..=23, r)]
    nominal: u8,
    /// The boost clock's level, or the core's preferred-core ranking on parts that have one.
    #[bits(24..=31, r)]
    highest: u8,
}

/// `CPPC_ENABLE`.
#[bitfield(u64, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct CppcEnable {
    #[bit(0, rw)]
    enable: bool,
}

/// `CPPC_REQ`: the range of levels the hardware picks from, and how it leans within it.
#[bitfield(u64, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct CppcRequest {
    #[bits(0..=7, rw)]
    maximum: u8,
    #[bits(8..=15, rw)]
    minimum: u8,
    /// 0 lets the hardware choose within the range.
    #[bits(16..=23, rw)]
    desired: u8,
    /// Energy-performance preference: 0 performance to 255 energy saving.
    #[bits(24..=31, rw)]
    energy_preference: u8,
}

register!(Hwcr, PStateLimit, PStateCtl, PStateStatus, PStateDef, CppcCapabilities, CppcEnable, CppcRequest);

/// Degrees Celsius from the SMN register `THM_TCON_CUR_TMP` (`0x5_9800`).
///
/// Bits 31:21 are eighths of a degree, less 49 degrees when the range-select bit (19) is set. All
/// ones is what a read of nothing returns. This is the control temperature (Tctl), which some parts
/// offset from the die's.
#[must_use]
pub fn tctl_celsius(raw: u32) -> Option<i16> {
    if raw == u32::MAX {
        return None;
    }
    let eighths = i32::try_from(raw >> 21).ok()?;
    let milli = eighths * 125 - if raw & (1 << 19) != 0 { 49_000 } else { 0 };
    i16::try_from((milli + 500).div_euclid(1000)).ok()
}
