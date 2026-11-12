//! DbC bring-up shared by the DbC apps: the preferred controller first, with the slow
//! controller listing done while the far host trains the link.

use alloc::vec::Vec;

use ferrite_efi::clock::Clock;
use ferrite_efi::dma::iommu_handle_count;
use ferrite_efi::link;
use ferrite_efi::xhci::{self, Controller, OpenDbc};
use ferrite_hal::{Clock as _, Instant, PciAddress};
use ferrite_xhci::{Config, State, XhciDevice};

use crate::display::{Chain, HexOrNone, Ranges};
use crate::summary;

/// Controller tried first: the Thunderbolt xHCI on the X1 Carbon.
pub const PREFERRED: PciAddress = PciAddress::new(0, 0x3b, 0, 0);

/// The listing of every xHCI on the machine, deferred so it runs while the link trains.
#[derive(Default)]
pub struct Survey {
    devices: Option<Vec<(Controller, XhciDevice)>>,
}

impl Survey {
    /// List and describe every xHCI, the first time only. It scans every bus, so it is slow.
    pub fn finish(&mut self) {
        self.devices();
    }

    /// Every xHCI on the machine, listing them first if that has not happened yet.
    pub fn devices(&mut self) -> &[(Controller, XhciDevice)] {
        self.devices.get_or_insert_with(list)
    }
}

fn list() -> Vec<(Controller, XhciDevice)> {
    let scan = match xhci::find_xhcis() {
        Ok(scan) => scan,
        Err(e) => {
            log::error!("PCI scan failed: {}", Chain(&e));
            return Vec::new();
        },
    };
    for failure in &scan.failures {
        log::warn!("PCI scan incomplete: {}", Chain(failure));
    }
    let mut devices: Vec<_> = scan
        .controllers
        .into_iter()
        .filter_map(|ctrl| match ctrl.probe() {
            Ok(dev) => Some((ctrl, dev)),
            Err(e) => {
                log::warn!("{}: probe failed: {}", ctrl.function.addr(), Chain(&e));
                None
            },
        })
        .collect();
    devices.sort_by_key(|(_, d)| (d.dbc_regs().is_none(), d.addr() != PREFERRED));
    log::info!("{} xHCI controller(s)", devices.len());
    for (_, dev) in &devices {
        describe(dev);
    }
    if devices.iter().all(|(_, d)| d.dbc_regs().is_none()) {
        log::info!("no xHCI with a Debug Capability (QEMU's qemu-xhci has none)");
    }
    devices
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

/// Whether the resident loader owns the DbC, which an app that opens it itself must not touch.
///
/// Logs the refusal, so the caller only has to return.
pub fn owned_by_loader(app: &str) -> bool {
    let owned = link::is_present();
    if owned {
        log::error!("{app} opens the DbC itself, but the loader owns it; run {app} without the loader");
    }
    owned
}

/// A configured DbC, and the controller listing if the link came up before it ran.
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

/// List the controllers if the link is still training, then wait out the configuration
/// deadline. On failure the DbC is disabled and dropped.
///
/// The listing runs before `wait_configured`, whose deadline and re-arm schedule start
/// when it returns, so a slow scan delays the first re-arm but cannot skip it.
fn settle(mut dbc: OpenDbc, survey: &mut Survey, clock: Clock, start: Instant) -> Option<OpenDbc> {
    let opened = clock.now();
    match iommu_handle_count() {
        Ok(n) => log::info!("IOMMU protocol handles: {n}"),
        Err(e) => log::warn!("IOMMU protocol lookup failed: {e}"),
    }
    dbc.poll(|_| {});
    if dbc.state() == State::Armed {
        survey.finish();
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
