//! DbC bring-up shared by the DbC apps: the preferred controller first, with the slow
//! survey work done between polls while the far host trains the link.

use alloc::vec::Vec;

use ferrite_efi::clock::Clock;
use ferrite_efi::dma::iommu_handle_count;
use ferrite_efi::xhci::{self, Controller, OpenDbc};
use ferrite_hal::{Clock as _, Instant, PciAddress};
use ferrite_xhci::{Config, State, XhciDevice};

use crate::display::{Chain, HexOrNone, Ranges};
use crate::summary;

/// Controller tried first: the Thunderbolt xHCI on the X1 Carbon.
pub const PREFERRED: PciAddress = PciAddress::new(0, 0x3b, 0, 0);

/// Slow reporting work, done one chore at a time so polling is never starved for long.
#[derive(Default)]
pub struct Survey {
    next: u8,
    listed: bool,
    devices: Vec<(Controller, XhciDevice)>,
}

const CHORES: u8 = 3;

impl Survey {
    #[must_use]
    pub const fn pending(&self) -> bool {
        self.next < CHORES
    }

    /// Run the next chore, if any. Call between polls.
    pub fn step(&mut self, dbc: &mut OpenDbc) {
        match self.next {
            0 => match iommu_handle_count() {
                Ok(n) => log::info!("IOMMU protocol handles: {n}"),
                Err(e) => log::warn!("IOMMU protocol lookup failed: {e}"),
            },
            1 => log::debug!("registers at arm:\n{}", dbc.snapshot()),
            2 => self.list(),
            _ => return,
        }
        self.next += 1;
    }

    /// Every xHCI on the machine, listing them first if that has not happened yet.
    pub fn devices(&mut self) -> &[(Controller, XhciDevice)] {
        self.list();
        &self.devices
    }

    fn list(&mut self) {
        if self.listed {
            return;
        }
        self.listed = true;
        let scan = match xhci::find_xhcis() {
            Ok(scan) => scan,
            Err(e) => {
                log::error!("PCI scan failed: {}", Chain(&e));
                return;
            },
        };
        for failure in &scan.failures {
            log::warn!("PCI scan incomplete: {}", Chain(failure));
        }
        for ctrl in scan.controllers {
            match ctrl.probe() {
                Ok(dev) => self.devices.push((ctrl, dev)),
                Err(e) => log::warn!("{}: probe failed: {}", ctrl.function.addr(), Chain(&e)),
            }
        }
        self.devices.sort_by_key(|(_, d)| (d.dbc_regs().is_none(), d.addr() != PREFERRED));
        log::info!("{} xHCI controller(s)", self.devices.len());
        for (_, dev) in &self.devices {
            describe(dev);
        }
        if self.devices.iter().all(|(_, d)| d.dbc_regs().is_none()) {
            log::info!("no xHCI with a Debug Capability (QEMU's qemu-xhci has none)");
        }
    }
}

fn describe(dev: &XhciDevice) {
    log::info!(
        "  {dev}  mmio {:#x}  caplength {:#x}  ports {}  dbc {}  legacy {}  usb3 ranges {}",
        dev.mmio(),
        dev.cap_length(),
        dev.max_ports(),
        HexOrNone(dev.dbc_regs()),
        HexOrNone(dev.legacy()),
        Ranges(dev.usb3_ports()),
    );
}

/// A configured DbC and the survey work still owed.
pub struct Link {
    pub dbc: OpenDbc,
    pub survey: Survey,
}

/// Open the DbC on the preferred controller and wait for the far host to configure it.
///
/// Every other controller with a DbC is tried after it. `start` is the reference for the
/// milestone times in failure reports.
pub fn bring_up(clock: Clock, start: Instant, cfg: &Config) -> Option<Link> {
    let mut survey = Survey::default();
    match xhci::open_at(PREFERRED, cfg, clock) {
        Ok(Some(dbc)) => {
            if let Some(dbc) = settle(dbc, &mut survey, clock, start) {
                return Some(Link { dbc, survey });
            }
        },
        Ok(None) => log::info!("no PCI root bridge decodes {PREFERRED}; scanning"),
        Err(xhci::Error::Driver { source: ferrite_xhci::Error::NoDbc }) => {
            log::info!("{PREFERRED} has no Debug Capability; scanning");
        },
        Err(e) => log::warn!("open {PREFERRED} failed: {}; trying the other controllers", Chain(&e)),
    }

    let others: Vec<(Controller, XhciDevice)> =
        survey.devices().iter().filter(|(_, d)| d.dbc_regs().is_some() && d.addr() != PREFERRED).cloned().collect();
    for (ctrl, dev) in &others {
        log::info!("trying {dev} (timeout {} ms)", cfg.timeout.as_millis());
        if let Ok(Some(before)) = ctrl.snapshot(dev) {
            log::info!("registers before:\n{before}");
        }
        match xhci::open(ctrl, cfg, clock) {
            Ok(dbc) => {
                if let Some(dbc) = settle(dbc, &mut survey, clock, start) {
                    return Some(Link { dbc, survey });
                }
            },
            Err(e) => log::warn!("open failed: {}", Chain(&e)),
        }
    }
    None
}

/// Do the pending survey work between polls while the link trains, then wait out the
/// rest of the configuration deadline. On failure the DbC is disabled and dropped.
fn settle(mut dbc: OpenDbc, survey: &mut Survey, clock: Clock, start: Instant) -> Option<OpenDbc> {
    let opened = clock.now();
    while dbc.state() == State::Armed && survey.pending() {
        dbc.poll(|_| {});
        survey.step(&mut dbc);
    }
    match dbc.wait_configured() {
        Ok(_) => {
            log::info!("CONFIGURED {} ms after open", clock.elapsed(opened).as_millis());
            Some(dbc)
        },
        Err(e) => {
            log::error!("FAILED: {}", Chain(&e));
            summary::report(&mut dbc, start);
            if let Err(e) = dbc.disable() {
                log::warn!("disable failed: {}", Chain(&e));
            }
            None
        },
    }
}
