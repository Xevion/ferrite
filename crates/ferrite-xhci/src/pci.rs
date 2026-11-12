//! PCI configuration space helpers: identification, BAR decoding, power state and
//! the decode and bus-master bits the controller needs for DMA.

use core::time::Duration;

use ferrite_hal::{Delay, PciAddress, PciConfig, PhysAddr};
use snafu::ResultExt;

use crate::error::{Error, PciConfigSnafu};

pub const ID: u8 = 0x00;
pub const COMMAND: u8 = 0x04;
pub const STATUS: u8 = 0x06;
pub const CLASS_REV: u8 = 0x08;
pub const BAR0: u8 = 0x10;
pub const BAR1: u8 = 0x14;
pub const CAP_PTR: u8 = 0x34;

pub const CMD_MEMORY: u16 = 1 << 1;
pub const CMD_BUS_MASTER: u16 = 1 << 2;
const STATUS_CAP_LIST: u16 = 1 << 4;

const CAP_ID_PM: u8 = 0x01;
const PMCSR_FROM_CAP: u8 = 4;
const PMCSR_STATE: u16 = 0x3;
/// `PME_Status` is write-one-to-clear; it must be written as zero to leave it alone.
const PMCSR_PME_STATUS: u16 = 1 << 15;
/// The capability list lives in the 192 bytes after the header, at dword granularity.
const CAP_WALK_LIMIT: usize = 48;
/// Recovery time a function needs after leaving D3hot before it may be accessed.
const D3_RECOVERY: Duration = Duration::from_millis(10);

/// Class code, subclass and programming interface of an xHCI controller.
pub const XHCI_CLASS: u32 = 0x0c_0330;

/// PCI power-management state and where to change it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PowerState {
    /// Config offset of PMCSR.
    pub pmcsr_offset: u8,
    /// 0 = D0 up to 3 = D3hot.
    pub state: u8,
}

/// Config space as found and as left by [`prepare`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prepared {
    pub command_before: u16,
    pub power_before: Option<PowerState>,
}

pub(crate) fn read8<H: PciConfig>(hal: &mut H, dev: PciAddress, offset: u8) -> Result<u8, Error> {
    hal.cfg_read8(dev, offset).context(PciConfigSnafu { op: "read8", dev, offset })
}

pub(crate) fn read16<H: PciConfig>(hal: &mut H, dev: PciAddress, offset: u8) -> Result<u16, Error> {
    hal.cfg_read16(dev, offset).context(PciConfigSnafu { op: "read16", dev, offset })
}

pub(crate) fn read32<H: PciConfig>(hal: &mut H, dev: PciAddress, offset: u8) -> Result<u32, Error> {
    hal.cfg_read32(dev, offset).context(PciConfigSnafu { op: "read32", dev, offset })
}

fn write16<H: PciConfig>(hal: &mut H, dev: PciAddress, offset: u8, value: u16) -> Result<(), Error> {
    hal.cfg_write16(dev, offset, value).context(PciConfigSnafu { op: "write16", dev, offset })
}

/// Whether BAR0's low dword describes a 64-bit memory BAR, so BAR1 holds the high half.
#[must_use]
pub const fn bar_is_64bit(lo: u32) -> bool {
    lo & 1 == 0 && lo & 0x6 == 0x4
}

/// Base address of a memory BAR; `None` for an I/O or unassigned BAR.
#[must_use]
pub fn decode_bar(lo: u32, hi: u32) -> Option<PhysAddr> {
    if lo & 1 != 0 {
        return None;
    }
    let hi = if bar_is_64bit(lo) { u64::from(hi) << 32 } else { 0 };
    let addr = hi | u64::from(lo & !0xf);
    (addr != 0).then_some(PhysAddr::new(addr))
}

/// Read BAR0 (and BAR1 when it is the high half).
pub fn bar0<H: PciConfig>(hal: &mut H, dev: PciAddress) -> Result<Option<PhysAddr>, Error> {
    let lo = read32(hal, dev, BAR0)?;
    let hi = if bar_is_64bit(lo) { read32(hal, dev, BAR1)? } else { 0 };
    Ok(decode_bar(lo, hi))
}

/// Walk the capability list for the power-management capability.
pub fn power_state<H: PciConfig>(hal: &mut H, dev: PciAddress) -> Result<Option<PowerState>, Error> {
    if read16(hal, dev, STATUS)? & STATUS_CAP_LIST == 0 {
        return Ok(None);
    }
    let mut ptr = read8(hal, dev, CAP_PTR)? & 0xfc;
    for _ in 0..CAP_WALK_LIMIT {
        if ptr == 0 {
            return Ok(None);
        }
        let cap = read16(hal, dev, ptr)?;
        if cap & 0xff == u16::from(CAP_ID_PM) {
            let Some(pmcsr_offset) = ptr.checked_add(PMCSR_FROM_CAP) else {
                log::warn!("{dev}: PM capability at {ptr:#04x} has no room for PMCSR");
                return Ok(None);
            };
            let state = (read16(hal, dev, pmcsr_offset)? & PMCSR_STATE) as u8;
            return Ok(Some(PowerState { pmcsr_offset, state }));
        }
        ptr = ((cap >> 8) & 0xfc) as u8;
    }
    log::warn!("{dev}: capability list did not end within {CAP_WALK_LIMIT} entries");
    Ok(None)
}

/// Bring the function to D0, then turn on memory decode and bus mastering.
///
/// Command register writes to a function in D3hot may be dropped, so the power
/// transition comes first.
pub fn prepare<H: PciConfig, D: Delay>(hal: &mut H, delay: &D, dev: PciAddress) -> Result<Prepared, Error> {
    let power_before = power_state(hal, dev)?;
    if let Some(pm) = power_before
        && pm.state != 0
    {
        let pmcsr = read16(hal, dev, pm.pmcsr_offset)?;
        write16(hal, dev, pm.pmcsr_offset, pmcsr & !(PMCSR_STATE | PMCSR_PME_STATUS))?;
        delay.delay(D3_RECOVERY);
        log::debug!("{dev}: moved from D{} to D0", pm.state);
    }
    let command_before = read16(hal, dev, COMMAND)?;
    let want = command_before | CMD_MEMORY | CMD_BUS_MASTER;
    if want != command_before {
        write16(hal, dev, COMMAND, want)?;
    }
    Ok(Prepared { command_before, power_before })
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};
    use ferrite_testing::{Sim, Write};
    use rstest::rstest;

    use super::*;

    const DEV: PciAddress = PciAddress::new(0, 0x3b, 0, 0);

    #[rstest]
    #[case::mem32(0xfe20_0000, 0xdead_beef, Some(0xfe20_0000))]
    #[case::mem32_prefetch_flags(0xfe20_0008, 0, Some(0xfe20_0000))]
    #[case::mem64(0x8000_0004, 0x60, Some(0x60_8000_0000))]
    #[case::mem64_high_only(0x0000_000c, 0x40, Some(0x40_0000_0000))]
    #[case::io(0x0000_e001, 0, None)]
    #[case::unassigned(0, 0, None)]
    fn bar_decoding(#[case] lo: u32, #[case] hi: u32, #[case] want: Option<u64>) {
        check!(decode_bar(lo, hi) == want.map(PhysAddr::new));
    }

    fn with_pm_chain(sim: &Sim, chain: &[(u8, u8, u8)]) {
        sim.cfg_poke16(DEV, STATUS, STATUS_CAP_LIST);
        sim.cfg_poke8(DEV, CAP_PTR, chain.first().map_or(0, |c| c.0));
        for &(at, id, next) in chain {
            sim.cfg_poke16(DEV, at, u16::from(id) | (u16::from(next) << 8));
        }
    }

    #[test]
    fn pm_cap_found_after_other_caps() {
        let sim = Sim::new();
        with_pm_chain(&sim, &[(0x50, 0x05, 0x70), (0x70, CAP_ID_PM, 0)]);
        sim.cfg_poke16(DEV, 0x74, 0x0103);
        let got = power_state(&mut sim.hal(), DEV).expect("config reads succeed");
        check!(got == Some(PowerState { pmcsr_offset: 0x74, state: 3 }));
    }

    #[rstest]
    #[case::no_cap_list(&[], false)]
    #[case::self_loop(&[(0x40, 0x05, 0x40)], true)]
    #[case::pm_at_top_has_no_pmcsr(&[(0xfc, CAP_ID_PM, 0)], true)]
    fn pm_cap_walk_terminates(#[case] chain: &[(u8, u8, u8)], #[case] cap_list: bool) {
        let sim = Sim::new();
        if cap_list {
            with_pm_chain(&sim, chain);
        }
        check!(power_state(&mut sim.hal(), DEV).expect("config reads succeed") == None);
    }

    #[test]
    fn prepare_wakes_before_enabling_and_never_sets_pme_status() {
        let sim = Sim::new();
        with_pm_chain(&sim, &[(0x70, CAP_ID_PM, 0)]);
        sim.cfg_poke16(DEV, 0x74, PMCSR_PME_STATUS | (1 << 8) | 3);
        sim.cfg_poke16(DEV, COMMAND, 0);
        let prep = prepare(&mut sim.hal(), &sim.clock(), DEV).expect("config access succeeds");
        check!(prep.command_before == 0);

        let writes = sim.writes();
        assert!(let [Write::Config { offset: 0x74, value: pmcsr, .. }, Write::Config { offset: COMMAND, value: cmd, .. }] = writes.as_slice());
        check!(*pmcsr == 1 << 8, "D0, PME_En kept, PME_Status written as zero");
        check!(*cmd == u32::from(CMD_MEMORY | CMD_BUS_MASTER));
        check!(sim.now() >= D3_RECOVERY);
    }

    #[test]
    fn prepare_writes_nothing_when_ready() {
        let sim = Sim::new();
        with_pm_chain(&sim, &[(0x70, CAP_ID_PM, 0)]);
        sim.cfg_poke16(DEV, COMMAND, CMD_MEMORY | CMD_BUS_MASTER);
        prepare(&mut sim.hal(), &sim.clock(), DEV).expect("config access succeeds");
        check!(sim.writes().is_empty());
    }

    #[test]
    fn config_failure_is_an_error() {
        let sim = Sim::new();
        sim.fail_cfg(DEV);
        check!(power_state(&mut sim.hal(), DEV).is_err());
    }
}
