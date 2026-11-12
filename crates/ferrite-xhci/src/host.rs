//! Host-controller side helpers the DbC bring-up needs: taking the controller
//! from the BIOS, and resetting idle USB 3 root ports.

use alloc::vec::Vec;
use core::time::Duration;

use ferrite_hal::{Clock, Delay, Mmio, MmioExt, PhysAddr, ReadWrite, Reg, RegValue, Rw1c, Rw1cWrite};
use snafu::ResultExt;

use crate::device::{PortNumber, XhciDevice};
use crate::error::{Error, MmioSnafu};
use crate::regs::{PortSc, host_portsc, xcap};

pub const INTEL_VENDOR: u16 = 0x8086;
/// How long the BIOS gets to release the controller before ownership is forced.
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(1);
const HANDOFF_POLL: Duration = Duration::from_micros(500);
/// Upper bound on waiting for reset ports to report Port Reset Change.
const PORT_RESET_TIMEOUT: Duration = Duration::from_millis(20);
const PORT_RESET_POLL: Duration = Duration::from_micros(100);

/// Result of [`bios_handoff`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Handoff {
    pub bios_owned: bool,
    /// The BIOS cleared its semaphore in time; false means ownership was forced.
    pub released: bool,
    pub waited: Duration,
}

/// One root port reset by [`reset_debug_ports`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortReset {
    pub port: PortNumber,
    pub before: PortSc,
    pub after: PortSc,
}

fn try_read<H: Mmio, T: RegValue, A: ferrite_hal::Readable>(
    hal: &mut H,
    base: PhysAddr,
    reg: Reg<T, A>,
) -> Result<T, Error> {
    hal.try_read(base, reg).context(MmioSnafu { op: "read", addr: reg.addr(base) })
}

fn try_write<H: Mmio, T: RegValue>(hal: &mut H, base: PhysAddr, reg: Reg<T, ReadWrite>, value: T) -> Result<(), Error> {
    hal.try_write(base, reg, value).context(MmioSnafu { op: "write", addr: reg.addr(base) })
}

fn try_write_rw1c<H: Mmio, T: RegValue>(
    hal: &mut H,
    base: PhysAddr,
    reg: Reg<T, Rw1c>,
    value: Rw1cWrite<T>,
) -> Result<(), Error> {
    hal.try_write_rw1c(base, reg, value).context(MmioSnafu { op: "write", addr: reg.addr(base) })
}

/// Claim the controller from the BIOS through the USB legacy support capability
/// at `legacy`, forcing it after a timeout, then turn off and acknowledge its SMIs.
pub fn bios_handoff<H: Mmio, C: Clock + Delay>(hal: &mut H, clock: &C, legacy: PhysAddr) -> Result<Handoff, Error> {
    let sup = try_read(hal, legacy, xcap::USBLEGSUP)?;
    let mut handoff = Handoff { bios_owned: sup.bios_owned(), released: true, waited: Duration::ZERO };
    if sup.bios_owned() {
        try_write(hal, legacy, xcap::USBLEGSUP, sup.with_os_owned(true))?;
        let t0 = clock.now();
        handoff.released = loop {
            if !try_read(hal, legacy, xcap::USBLEGSUP)?.bios_owned() {
                break true;
            }
            if clock.elapsed(t0) >= HANDOFF_TIMEOUT {
                break false;
            }
            clock.delay(HANDOFF_POLL);
        };
        handoff.waited = clock.elapsed(t0);
        if !handoff.released {
            let now = try_read(hal, legacy, xcap::USBLEGSUP)?;
            try_write(hal, legacy, xcap::USBLEGSUP, now.with_bios_owned(false))?;
        }
    }
    let ctl = try_read(hal, legacy, xcap::USBLEGCTLSTS)?;
    try_write_rw1c(hal, legacy, xcap::USBLEGCTLSTS, ctl.disable_smis())?;
    Ok(handoff)
}

/// Reset every unconnected USB 3 root port and wait, bounded, for the resets to finish.
///
/// Intel parts retrain a debug link that came up before DCE was set this way;
/// other vendors are left alone.
pub fn reset_debug_ports<H: Mmio, C: Clock + Delay>(
    hal: &mut H,
    clock: &C,
    dev: &XhciDevice,
) -> Result<Vec<PortReset>, Error> {
    if dev.vendor() != INTEL_VENDOR {
        return Ok(Vec::new());
    }
    let op = dev.op_base();
    let mut resets = Vec::new();
    for port in dev.usb3_ports().iter().flat_map(|r| r.ports()) {
        let reg = host_portsc(port);
        let before = try_read(hal, op, reg)?;
        if before.raw_value() != u32::MAX && !before.ccs() {
            try_write_rw1c(hal, op, reg, before.host_reset())?;
            resets.push(PortReset { port, before, after: before });
        }
    }
    if resets.is_empty() {
        return Ok(resets);
    }
    let t0 = clock.now();
    loop {
        for r in &mut resets {
            r.after = try_read(hal, op, host_portsc(r.port))?;
        }
        if resets.iter().all(|r| r.after.prc()) || clock.elapsed(t0) >= PORT_RESET_TIMEOUT {
            return Ok(resets);
        }
        clock.delay(PORT_RESET_POLL);
    }
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};
    use ferrite_hal::PciAddress;
    use ferrite_testing::{Sim, Write};

    use super::*;
    use crate::device::XhciFunction;

    const DEV: PciAddress = PciAddress::new(0, 0x3b, 0, 0);
    const MMIO: u64 = 0xf000_0000;
    const OP: u64 = MMIO + 0x20;
    const LEGACY: u64 = MMIO + 0x800;

    fn device(sim: &Sim, usb3: u32) -> XhciDevice {
        sim.add_xhci(DEV, MMIO);
        sim.poke(MMIO, 0x0100_0020);
        sim.poke(MMIO + 4, 4 << 24);
        sim.poke(MMIO + 0x10, (0x800 / 4) << 16);
        sim.poke(LEGACY, 0x0000_0401);
        sim.poke(LEGACY + 0x10, 0x0300_0002);
        sim.poke(LEGACY + 0x18, usb3);
        let func = XhciFunction::identify(&mut sim.hal(), DEV).expect("config reads").expect("an xHCI");
        XhciDevice::probe(&mut sim.hal(), func).expect("probe")
    }

    fn portsc(port: u64) -> u64 {
        OP + 0x400 + (port - 1) * 0x10
    }

    #[test]
    fn resets_only_idle_usb3_ports_with_the_preserve_mask() {
        let sim = Sim::new();
        let dev = device(&sim, 0x0000_0301);
        sim.poke(portsc(1), 0x0e00_02a0 | (1 << 17) | (1 << 1));
        sim.poke(portsc(2), 0x0000_1203);
        sim.poke(portsc(3), 0x0000_02a0);
        sim.poke(portsc(4), 0x0000_02a0);
        sim.clear_writes();

        let resets = reset_debug_ports(&mut sim.hal(), &sim.clock(), &dev).expect("MMIO works");
        let ports: Vec<_> = resets.iter().map(|r| r.port.get()).collect();
        check!(ports == [1, 3], "connected port 2 and USB 2 port 4 are skipped");
        check!(
            sim.writes()
                == [
                    Write::Mmio { addr: portsc(1), value: 0x0e00_0210 },
                    Write::Mmio { addr: portsc(3), value: 0x0000_0210 },
                ]
        );
    }

    #[test]
    fn port_reset_returns_once_every_port_reports_prc() {
        let sim = Sim::new();
        let dev = device(&sim, 0x0000_0201);
        sim.poke(portsc(1), 0x0000_02a0);
        sim.poke(portsc(2), 0x0000_02a0);
        sim.set_host_reset_delay(Duration::from_millis(2));
        let resets = reset_debug_ports(&mut sim.hal(), &sim.clock(), &dev).expect("MMIO works");
        check!(resets.iter().all(|r| r.after.prc()));
        check!(sim.now() < PORT_RESET_TIMEOUT, "no fixed stall once the ports are done");
    }

    #[test]
    fn handoff_forces_ownership_after_timeout_and_disables_smis() {
        let sim = Sim::new();
        let _dev = device(&sim, 0x0000_0201);
        sim.poke(LEGACY, 0x0001_0401);
        sim.poke(LEGACY + 4, 0xe00f_e01f);
        sim.clear_writes();
        let h = bios_handoff(&mut sim.hal(), &sim.clock(), PhysAddr::new(LEGACY)).expect("MMIO works");
        check!(h.bios_owned);
        check!(!h.released);
        check!(h.waited >= HANDOFF_TIMEOUT);
        let writes = sim.writes();
        assert!(let [claim, force, smi] = writes.as_slice());
        check!(*claim == Write::Mmio { addr: LEGACY, value: 0x0101_0401 });
        check!(*force == Write::Mmio { addr: LEGACY, value: 0x0100_0401 });
        check!(*smi == Write::Mmio { addr: LEGACY + 4, value: 0xe00e_0006 });
    }
}
