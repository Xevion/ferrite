//! The PCI devices the platform hands to the drivers, for the shadow `drivers/pci` C, and the cadence
//! of polled interrupts.
//!
//! The platform reports a fixed list of functions; an index into it names a device in every call. The C
//! builds one `struct pci_dev` per entry and asks here for config space, memory decode and bus mastering.
//! BARs are mapped by the C as their CPU address (identity under UEFI). There are no interrupt lines: the
//! IRQ task runs every requested handler when the platform's poll cadence says so.

use core::cell::UnsafeCell;
use core::ffi::{c_int, c_uint, c_ulong, c_void};
use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;

use ferrite_hal::{Instant, PciAddress, PhysAddr};
use snafu::Snafu;

use crate::task::installed;
use crate::trace_and_stop;

const EIO: c_int = 5;
const ENODEV: c_int = 19;
const EINVAL: c_int = 22;

/// What a BAR decodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarKind {
    Memory32,
    Memory64,
    Io,
}

/// One assigned BAR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bar {
    pub base: PhysAddr,
    pub size: u64,
    pub kind: BarKind,
    pub prefetchable: bool,
}

/// A function as the platform found it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PciDevice {
    pub location: PciAddress,
    pub vendor: u16,
    pub device: u16,
    pub subsystem_vendor: u16,
    pub subsystem_device: u16,
    /// The 24-bit class code (class, subclass, programming interface).
    pub class: u32,
    pub revision: u8,
    pub header_type: u8,
    /// Indexed by BAR number; the upper half of a 64-bit BAR is `None`.
    pub bars: [Option<Bar>; 6],
}

impl fmt::Display for PciDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {:04x}:{:04x} (subsystem {:04x}:{:04x}, class {:06x}, rev {:02x})",
            self.location,
            self.vendor,
            self.device,
            self.subsystem_vendor,
            self.subsystem_device,
            self.class,
            self.revision
        )
    }
}

/// A config access width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Width {
    U8,
    U16,
    U32,
}

impl Width {
    const fn from_bytes(bytes: c_uint) -> Option<Self> {
        match bytes {
            1 => Some(Self::U8),
            2 => Some(Self::U16),
            4 => Some(Self::U32),
            _ => None,
        }
    }

    pub const fn bytes(self) -> u16 {
        match self {
            Self::U8 => 1,
            Self::U16 => 2,
            Self::U32 => 4,
        }
    }
}

#[derive(Debug, Snafu)]
pub enum PciError {
    #[snafu(display("no device {index}"))]
    NoDevice { index: usize },
    #[snafu(display("{width:?} config access at {offset:#x} of {location} failed: {reason}"))]
    Config { location: PciAddress, offset: u16, width: Width, reason: alloc::string::String },
    #[snafu(display("{what} on {location} failed: {reason}"))]
    Control { location: PciAddress, what: &'static str, reason: alloc::string::String },
}

/// Where PCI devices come from on this platform. Calls arrive from the one thread that runs the C.
pub trait PciPlatform {
    /// The devices handed to the drivers, in a fixed order.
    fn devices(&self) -> &[PciDevice];
    fn config_read(&self, index: usize, offset: u16, width: Width) -> Result<u32, PciError>;
    fn config_write(&self, index: usize, offset: u16, width: Width, value: u32) -> Result<(), PciError>;
    /// Turn on decoding of the device's BARs, as `pci_enable_device` does.
    fn enable(&self, index: usize) -> Result<(), PciError>;
    fn set_bus_master(&self, index: usize, on: bool) -> Result<(), PciError>;
}

struct Slot(UnsafeCell<Option<&'static dyn PciPlatform>>);

// SAFETY: written once by `install` before any C runs, then only read, from the one thread that runs
// the C and the main loop.
unsafe impl Sync for Slot {}

static PLATFORM: Slot = Slot(UnsafeCell::new(None));
static INSTALLED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Snafu)]
#[snafu(display("a PCI platform is already installed"))]
pub struct PciAlreadyInstalled;

/// Hand the platform's devices to the drivers, once per program, before the C runs.
pub fn install(platform: &'static dyn PciPlatform) -> Result<(), PciAlreadyInstalled> {
    if INSTALLED.swap(true, Ordering::AcqRel) {
        return Err(PciAlreadyInstalled);
    }
    // SAFETY: the flag lets one caller in, and nothing reads the slot before the C runs.
    unsafe { *PLATFORM.0.get() = Some(platform) };
    Ok(())
}

fn platform() -> Option<&'static dyn PciPlatform> {
    if !INSTALLED.load(Ordering::Acquire) {
        return None;
    }
    // SAFETY: see `Slot`.
    unsafe { *PLATFORM.0.get() }
}

/// How many devices the drivers were given; zero when no platform is installed.
pub fn device_count() -> usize {
    platform().map_or(0, |p| p.devices().len())
}

/// The command register's bus master enable bit.
const COMMAND_MASTER: u32 = 1 << 2;

/// Clear bus mastering on every device, so nothing DMAs once the Linux side is stopped, and read the
/// command register back to show it. Safe to call from the halt hook; failures are logged.
pub fn park_all() {
    let Some(platform) = platform() else { return };
    for (index, device) in platform.devices().iter().enumerate() {
        if let Err(error) = platform.set_bus_master(index, false) {
            log::error!("parking {} failed: {error}", device.location);
        }
        match platform.config_read(index, 0x04, Width::U16) {
            Ok(command) if command & COMMAND_MASTER == 0 => {
                log::info!("parked {}: command register {command:#06x}, bus mastering off", device.location);
            },
            Ok(command) => {
                log::error!("{} still has bus mastering on: command register {command:#06x}", device.location);
            },
            Err(error) => log::error!("reading back the command register of {} failed: {error}", device.location),
        }
    }
}

/// When the IRQ task is woken in polled mode: at most once per `period`, and only once a device exists.
#[derive(Clone, Copy, Debug)]
pub struct PollCadence {
    period: Duration,
    last: Option<Instant>,
}

impl PollCadence {
    pub const fn new(period: Duration) -> Self {
        Self { period, last: None }
    }

    /// Whether a poll is due at `now`; a due poll restarts the period.
    pub fn due(&mut self, now: Instant) -> bool {
        let due = self.last.is_none_or(|last| now.saturating_duration_since(last) >= self.period);
        if due {
            self.last = Some(now);
        }
        due
    }

    /// The latest time the main loop may sleep to before the next poll is due.
    pub fn next(&self) -> Option<Instant> {
        self.last.and_then(|last| last.checked_add(self.period))
    }
}

/// `struct lx_pci_bar` of the shadow C.
#[repr(C)]
pub struct LxBar {
    start: u64,
    size: u64,
    /// 0 none, 1 memory 32, 2 memory 64, 3 I/O; bit 8 prefetchable.
    flags: u32,
    _pad: u32,
}

/// `struct lx_pci_info` of the shadow C.
#[repr(C)]
pub struct LxPciInfo {
    segment: u16,
    bus: u8,
    devfn: u8,
    vendor: u16,
    device: u16,
    subsystem_vendor: u16,
    subsystem_device: u16,
    class: u32,
    revision: u8,
    header_type: u8,
    _pad: u16,
    bars: [LxBar; 6],
}

const fn lx_bar(bar: Option<Bar>) -> LxBar {
    match bar {
        None => LxBar { start: 0, size: 0, flags: 0, _pad: 0 },
        Some(bar) => {
            let kind = match bar.kind {
                BarKind::Memory32 => 1,
                BarKind::Memory64 => 2,
                BarKind::Io => 3,
            };
            let prefetch = if bar.prefetchable { 0x100 } else { 0 };
            LxBar { start: bar.base.get(), size: bar.size, flags: kind | prefetch, _pad: 0 }
        },
    }
}

/// The number of devices the C should create.
#[unsafe(no_mangle)]
pub extern "sysv64" fn lx_pci_device_count() -> c_ulong {
    device_count() as c_ulong
}

/// Fill `info` for device `index`; 0, or `-ENODEV` past the end.
///
/// # Safety
/// `info` must be valid for writes.
#[unsafe(no_mangle)]
pub unsafe extern "sysv64" fn lx_pci_device_info(index: c_ulong, info: *mut LxPciInfo) -> c_int {
    let Some(device) = platform().and_then(|p| p.devices().get(index as usize)) else { return -ENODEV };
    let at = device.location;
    let lx = LxPciInfo {
        segment: at.segment(),
        bus: at.bus(),
        devfn: (at.device() << 3) | at.function(),
        vendor: device.vendor,
        device: device.device,
        subsystem_vendor: device.subsystem_vendor,
        subsystem_device: device.subsystem_device,
        class: device.class,
        revision: device.revision,
        header_type: device.header_type,
        _pad: 0,
        bars: device.bars.map(lx_bar),
    };
    // SAFETY: the caller guarantees `info` is writable.
    unsafe { info.write(lx) };
    0
}

fn with_platform<T>(call: &'static core::ffi::CStr, f: impl FnOnce(&dyn PciPlatform) -> T) -> T {
    let Some(platform) = platform() else {
        log::error!("{} called with no PCI platform installed", call.to_string_lossy());
        trace_and_stop(call)
    };
    f(platform)
}

/// Read `bytes` (1, 2 or 4) of device `index`'s config space at `offset` into `*value`; 0 or `-EIO`.
///
/// # Safety
/// `value` must be valid for writes.
#[unsafe(no_mangle)]
pub unsafe extern "sysv64" fn lx_pci_config_read(
    index: c_ulong,
    offset: c_uint,
    bytes: c_uint,
    value: *mut u32,
) -> c_int {
    let (Some(width), Ok(offset)) = (Width::from_bytes(bytes), u16::try_from(offset)) else { return -EINVAL };
    with_platform(c"lx_pci_config_read", |p| match p.config_read(index as usize, offset, width) {
        Ok(read) => {
            // SAFETY: the caller guarantees `value` is writable.
            unsafe { value.write(read) };
            0
        },
        Err(error) => {
            log::warn!("{error}");
            // SAFETY: as above; a failed read returns all ones, as a master abort does.
            unsafe { value.write(u32::MAX) };
            -EIO
        },
    })
}

/// Write the low `bytes` of `value` to device `index`'s config space at `offset`; 0 or `-EIO`.
#[unsafe(no_mangle)]
pub extern "sysv64" fn lx_pci_config_write(index: c_ulong, offset: c_uint, bytes: c_uint, value: u32) -> c_int {
    let (Some(width), Ok(offset)) = (Width::from_bytes(bytes), u16::try_from(offset)) else { return -EINVAL };
    with_platform(c"lx_pci_config_write", |p| match p.config_write(index as usize, offset, width, value) {
        Ok(()) => 0,
        Err(error) => {
            log::warn!("{error}");
            -EIO
        },
    })
}

/// Enable decoding of device `index`'s BARs; 0 or `-EIO`.
#[unsafe(no_mangle)]
pub extern "sysv64" fn lx_pci_enable(index: c_ulong) -> c_int {
    with_platform(c"lx_pci_enable", |p| match p.enable(index as usize) {
        Ok(()) => 0,
        Err(error) => {
            log::error!("{error}");
            -EIO
        },
    })
}

/// Turn bus mastering of device `index` on or off; 0 or `-EIO`.
#[unsafe(no_mangle)]
pub extern "sysv64" fn lx_pci_set_master(index: c_ulong, on: c_int) -> c_int {
    with_platform(c"lx_pci_set_master", |p| match p.set_bus_master(index as usize, on != 0) {
        Ok(()) => {
            log::info!("PCI device {index}: bus mastering {}", if on != 0 { "on" } else { "off" });
            0
        },
        Err(error) => {
            log::error!("{error}");
            -EIO
        },
    })
}

/// Make the task of `task` the one woken when the platform's poll cadence says interrupts are pending.
#[unsafe(no_mangle)]
pub extern "sysv64" fn lx_irq_set_task(task: *mut c_void) {
    let sched = &installed(c"lx_irq_set_task").sched;
    let Some(id) = sched.find_by_user(task as usize) else {
        log::error!("IRQ task {task:p} has no scheduler task");
        trace_and_stop(c"lx_irq_set_task");
    };
    sched.set_irq_task(id);
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    #[test]
    fn polls_are_spaced_by_the_period() {
        let at = |ms| Instant::from_epoch(Duration::from_millis(ms));
        let mut cadence = PollCadence::new(Duration::from_millis(1));
        check!(cadence.next().is_none());
        check!(cadence.due(at(5)));
        check!(!cadence.due(at(5)));
        check!(cadence.next() == Some(at(6)));
        check!(cadence.due(at(6)));
        check!(cadence.due(at(9)));
        check!(cadence.next() == Some(at(10)));
    }
}
