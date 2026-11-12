//! Finding xHCI controllers through the firmware's root bridges and opening a
//! DbC on one, with DCE cleared again at `ExitBootServices`.

use alloc::vec::Vec;
use core::ffi::c_void;
use core::ops::{Deref, DerefMut};
use core::ptr::NonNull;

use ferrite_hal::{PciAddress, PhysAddr};
use ferrite_xhci::{Config, Dbc, RegSnapshot, XhciDevice, XhciFunction};
use snafu::{OptionExt, ResultExt, Snafu, ensure};
use uefi::boot::{self, EventType, Tpl};
use uefi::proto::pci::root_bridge::PciRootBridgeIo;
use uefi::{Event, Handle};

use crate::clock::Clock;
use crate::dma::{BootPages, BootServicesDma};
use crate::link;
use crate::pci::RootBridge;

/// The DbC driver on the UEFI backends.
pub type EfiDbc = Dbc<RootBridge, Clock, BootPages>;

#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum Error {
    #[snafu(display("locating PCI root bridges failed"))]
    Locate { source: uefi::Error },

    #[snafu(display("opening the root bridge failed"))]
    OpenBridge { source: uefi::Error },

    #[snafu(display("enumerating root bridge segment {segment} failed"))]
    Enumerate { segment: u16, source: uefi::Error },

    #[snafu(display("{addr} is not an xHCI"))]
    NotXhci { addr: PciAddress },

    #[snafu(display("xHCI driver"))]
    Driver { source: ferrite_xhci::Error },

    #[snafu(display("registering the ExitBootServices hook failed"))]
    ExitHook { source: uefi::Error },

    #[snafu(display("a loader already owns the DbC and apps must use its link"))]
    LinkOwned,
}

/// An xHCI function and the root bridge it sits behind.
#[derive(Clone, Debug)]
pub struct Controller {
    pub bridge: Handle,
    pub function: XhciFunction,
}

impl Controller {
    /// Read its capabilities without side effects; fails while decode is off.
    pub fn probe(&self) -> Result<XhciDevice, Error> {
        let mut rb = RootBridge::open(self.bridge).context(OpenBridgeSnafu)?;
        XhciDevice::probe(&mut rb, self.function.clone()).context(DriverSnafu)
    }

    /// Read the DbC registers without touching them; `None` when there is no DbC.
    pub fn snapshot(&self, dev: &XhciDevice) -> Result<Option<RegSnapshot>, Error> {
        let Some(regs) = dev.dbc_regs() else {
            return Ok(None);
        };
        let mut rb = RootBridge::open(self.bridge).context(OpenBridgeSnafu)?;
        Ok(Some(RegSnapshot::read(&mut rb, regs)))
    }
}

/// What a bus scan found, plus whatever stopped part of it.
#[derive(Debug, Default)]
pub struct Scan {
    pub controllers: Vec<Controller>,
    /// Bridges or functions that could not be read; their absence from
    /// `controllers` does not mean there is no xHCI there.
    pub failures: Vec<Error>,
}

/// Every xHCI function on every root bridge, deduplicated by address. Reads only.
pub fn find_xhcis() -> Result<Scan, Error> {
    let mut scan = Scan::default();
    for handle in boot::find_handles::<PciRootBridgeIo>().context(LocateSnafu)? {
        let mut rb = match RootBridge::open(handle) {
            Ok(rb) => rb,
            Err(source) => {
                scan.failures.push(Error::OpenBridge { source });
                continue;
            },
        };
        let addrs = match rb.enumerate() {
            Ok(addrs) => addrs,
            Err(source) => {
                scan.failures.push(Error::Enumerate { segment: rb.segment(), source });
                continue;
            },
        };
        for addr in addrs {
            if scan.controllers.iter().any(|c| c.function.addr() == addr) {
                continue;
            }
            match XhciFunction::identify(&mut rb, addr) {
                Ok(Some(function)) => scan.controllers.push(Controller { bridge: handle, function }),
                Ok(None) => {},
                Err(source) => scan.failures.push(Error::Driver { source }),
            }
        }
    }
    Ok(scan)
}

/// The xHCI at `addr`, checking only the bridge whose bus range covers it
/// rather than scanning every bus. `Ok(None)` when no bridge decodes `addr`.
pub fn find_xhci(addr: PciAddress) -> Result<Option<Controller>, Error> {
    for handle in boot::find_handles::<PciRootBridgeIo>().context(LocateSnafu)? {
        let mut rb = RootBridge::open(handle).context(OpenBridgeSnafu)?;
        if rb.segment() != addr.segment() {
            continue;
        }
        let ranges = rb.bus_ranges().context(EnumerateSnafu { segment: rb.segment() })?;
        if !ranges.iter().any(|r| r.contains(&addr.bus())) {
            continue;
        }
        let function = XhciFunction::identify(&mut rb, addr).context(DriverSnafu)?.context(NotXhciSnafu { addr })?;
        return Ok(Some(Controller { bridge: handle, function }));
    }
    Ok(None)
}

/// Bring the controller up for DMA, probe it and open its DbC.
///
/// Refused while a loader's link is installed: two drivers on one DbC would corrupt both.
pub fn open(ctrl: &Controller, cfg: &Config, clock: Clock) -> Result<OpenDbc, Error> {
    ensure!(!link::is_present(), LinkOwnedSnafu);
    let mut rb = RootBridge::open(ctrl.bridge).context(OpenBridgeSnafu)?;
    let mut function = ctrl.function.clone();
    if !function.decoding() {
        ferrite_xhci::pci::prepare(&mut rb, &clock, function.addr()).context(DriverSnafu)?;
        let addr = function.addr();
        function = XhciFunction::identify(&mut rb, addr).context(DriverSnafu)?.context(NotXhciSnafu { addr })?;
    }
    let dev = XhciDevice::probe(&mut rb, function).context(DriverSnafu)?;
    let dbc = Dbc::open(rb, clock, &mut BootServicesDma, &dev, cfg).context(DriverSnafu)?;
    // On failure `dbc` drops here, which clears DCE before the memory goes.
    let hook = ExitHook::arm(dbc.dcctrl_addr()).context(ExitHookSnafu)?;
    Ok(OpenDbc { dbc, hook: Some(hook) })
}

/// [`find_xhci`] then [`open`], for the fastest bring-up on a known machine.
pub fn open_at(addr: PciAddress, cfg: &Config, clock: Clock) -> Result<Option<OpenDbc>, Error> {
    find_xhci(addr)?.map(|ctrl| open(&ctrl, cfg, clock)).transpose()
}

/// An open DbC whose DCE is also cleared if boot services exit while it is open.
pub struct OpenDbc {
    dbc: EfiDbc,
    hook: Option<ExitHook>,
}

impl Deref for OpenDbc {
    type Target = EfiDbc;

    fn deref(&self) -> &EfiDbc {
        &self.dbc
    }
}

impl DerefMut for OpenDbc {
    fn deref_mut(&mut self) -> &mut EfiDbc {
        &mut self.dbc
    }
}

impl Drop for OpenDbc {
    fn drop(&mut self) {
        let hook = self.hook.take();
        if self.dbc.release().is_err() {
            // DCE may still be set over leaked memory: keep the hook so
            // ExitBootServices gets one more chance to clear it.
            core::mem::forget(hook);
        }
    }
}

/// A `SIGNAL_EXIT_BOOT_SERVICES` event that writes zero to DCCTRL.
struct ExitHook {
    event: Option<Event>,
}

impl ExitHook {
    fn arm(dcctrl: PhysAddr) -> uefi::Result<Self> {
        let ctx = NonNull::new(dcctrl.get() as *mut c_void);
        // SAFETY: the notify function only performs one MMIO store to the
        // address it is given, which needs no boot services, allocation or locks.
        let event =
            unsafe { boot::create_event(EventType::SIGNAL_EXIT_BOOT_SERVICES, Tpl::NOTIFY, Some(clear_dce), ctx) }?;
        Ok(Self { event: Some(event) })
    }
}

impl Drop for ExitHook {
    fn drop(&mut self) {
        if let Some(event) = self.event.take()
            && let Err(e) = boot::close_event(event)
        {
            log::warn!("closing the ExitBootServices hook failed: {e}");
        }
    }
}

/// Clear DCE (all-zero DCCTRL, as `DcCtrl::disable`) directly, since protocol
/// calls are off limits inside an `ExitBootServices` notification.
#[expect(
    clippy::disallowed_methods,
    reason = "MMIO store from the ExitBootServices notify; no protocol is usable here"
)]
unsafe extern "efiapi" fn clear_dce(_event: Event, ctx: Option<NonNull<c_void>>) {
    if let Some(dcctrl) = ctx {
        // SAFETY: `ctx` is DCCTRL's physical address from an open DbC; boot-services
        // page tables map MMIO 1:1, and the register is a naturally aligned dword.
        unsafe { core::ptr::write_volatile(dcctrl.cast::<u32>().as_ptr(), 0) };
    }
}
