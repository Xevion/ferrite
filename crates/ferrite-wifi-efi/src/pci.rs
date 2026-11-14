//! The Wi-Fi cards the drivers get, found through the firmware's PCI I/O protocol: config space,
//! BARs, memory decode and bus mastering all go through the device's own `EFI_PCI_IO_PROTOCOL`.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::Cell;
use core::ffi::c_void;
use core::mem::ManuallyDrop;
use core::ptr::NonNull;

use ferrite_hal::{PciAddress, PhysAddr};
use ferrite_lkpi::pci::{Bar, BarKind, PciDevice, PciError, PciPlatform, Width};
use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams, ScopedProtocol};
use uefi::proto::unsafe_protocol;
use uefi::{Handle, Status, StatusExt as _};

/// A card the drivers are given, by vendor and device ID.
pub struct Wanted {
    pub vendor: u16,
    pub device: u16,
    pub name: &'static str,
    /// The firmware file its driver loads at probe, under its Linux name.
    pub firmware: &'static str,
}

/// The devices handed to the drivers.
pub const WANTED: [Wanted; 1] =
    [Wanted { vendor: 0x8086, device: 0x24fd, name: "Intel Wireless 8265", firmware: "iwlwifi-8265-36.ucode" }];

/// `EFI_PCI_IO_PROTOCOL_WIDTH`.
const WIDTH_U8: u32 = 0;
const WIDTH_U16: u32 = 1;
const WIDTH_U32: u32 = 2;

/// `EFI_PCI_IO_PROTOCOL_ATTRIBUTE_OPERATION`.
const ATTR_GET: u32 = 0;
const ATTR_ENABLE: u32 = 2;
const ATTR_DISABLE: u32 = 3;
const ATTR_SUPPORTED: u32 = 4;
/// `EFI_PCI_IO_ATTRIBUTE_*`.
pub const ATTR_MEMORY: u64 = 0x0200;
pub const ATTR_BUS_MASTER: u64 = 0x0400;

/// `EFI_PCI_IO_PROTOCOL_OPERATION`: shared by CPU and device for the mapping's whole life.
pub const MAP_COMMON_BUFFER: u32 = 2;

type ConfigAccess = unsafe extern "efiapi" fn(*const PciIo, u32, u32, usize, *mut c_void) -> Status;

/// `EFI_PCI_IO_PROTOCOL`. Members never called are plain words, so the layout matches.
#[repr(C)]
#[unsafe_protocol("4cf5b200-68b8-4ca5-9eec-b23e3f50029a")]
pub struct PciIo {
    poll_mem: usize,
    poll_io: usize,
    mem_read: usize,
    mem_write: usize,
    io_read: usize,
    io_write: usize,
    pci_read: ConfigAccess,
    pci_write: ConfigAccess,
    copy_mem: usize,
    pub map: unsafe extern "efiapi" fn(*const Self, u32, *mut c_void, *mut usize, *mut u64, *mut *mut c_void) -> Status,
    pub unmap: unsafe extern "efiapi" fn(*const Self, *mut c_void) -> Status,
    pub allocate_buffer: unsafe extern "efiapi" fn(*const Self, u32, u32, usize, *mut *mut c_void, u64) -> Status,
    pub free_buffer: unsafe extern "efiapi" fn(*const Self, usize, *mut c_void) -> Status,
    flush: usize,
    get_location: unsafe extern "efiapi" fn(*const Self, *mut usize, *mut usize, *mut usize, *mut usize) -> Status,
    attributes: unsafe extern "efiapi" fn(*const Self, u32, u64, *mut u64) -> Status,
    get_bar_attributes: unsafe extern "efiapi" fn(*const Self, u8, *mut u64, *mut *mut c_void) -> Status,
    set_bar_attributes: usize,
    rom_size: u64,
    rom_image: *mut c_void,
}

impl PciIo {
    fn config_read(&self, offset: u32, width: Width) -> uefi::Result<u32> {
        let mut value = 0_u32;
        let code = match width {
            Width::U8 => WIDTH_U8,
            Width::U16 => WIDTH_U16,
            Width::U32 => WIDTH_U32,
        };
        // SAFETY: one element of `width` is read into a u32, which is large enough and zeroed above.
        unsafe { (self.pci_read)(self, code, offset, 1, (&raw mut value).cast()) }.to_result()?;
        Ok(value)
    }

    fn config_write(&self, offset: u32, width: Width, value: u32) -> uefi::Result {
        let mut value = value;
        let code = match width {
            Width::U8 => WIDTH_U8,
            Width::U16 => WIDTH_U16,
            Width::U32 => WIDTH_U32,
        };
        // SAFETY: one element of `width` is written from the low bytes of a u32 (little endian).
        unsafe { (self.pci_write)(self, code, offset, 1, (&raw mut value).cast()) }.to_result()
    }

    fn location(&self) -> uefi::Result<PciAddress> {
        let (mut seg, mut bus, mut dev, mut fun) = (0, 0, 0, 0);
        // SAFETY: four writable usizes.
        unsafe { (self.get_location)(self, &raw mut seg, &raw mut bus, &raw mut dev, &raw mut fun) }.to_result()?;
        let narrow = |v: usize| u8::try_from(v).ok();
        let found = u16::try_from(seg).ok().zip(narrow(bus)).zip(narrow(dev)).zip(narrow(fun));
        found
            .and_then(|(((seg, bus), dev), fun)| PciAddress::checked(seg, bus, dev, fun))
            .ok_or_else(|| uefi::Error::from(Status::DEVICE_ERROR))
    }

    fn attributes(&self, operation: u32, attributes: u64) -> uefi::Result<u64> {
        let mut result = 0_u64;
        // SAFETY: `result` is writable; the firmware validates the operation and bits.
        unsafe { (self.attributes)(self, operation, attributes, &raw mut result) }.to_result()?;
        Ok(result)
    }

    /// Base and length of BAR `index` from the firmware's resource descriptor.
    fn bar_resource(&self, index: u8) -> uefi::Result<Option<(u64, u64)>> {
        let mut supports = 0_u64;
        let mut resources: *mut c_void = core::ptr::null_mut();
        // SAFETY: both outputs are writable; the firmware allocates the descriptor list from pool.
        unsafe { (self.get_bar_attributes)(self, index, &raw mut supports, &raw mut resources) }.to_result()?;
        let Some(list) = NonNull::new(resources.cast::<u8>()) else { return Ok(None) };
        // SAFETY: the firmware returned an ACPI resource list that ends with an end tag.
        let found = unsafe { qword_descriptor(list.as_ptr()) };
        // SAFETY: allocated by the firmware from pool for the caller to free, once.
        if let Err(error) = unsafe { boot::free_pool(list) } {
            log::debug!("freeing a BAR descriptor failed: {error}");
        }
        Ok(found)
    }
}

/// The first ACPI QWORD address space descriptor in a list: (base, length).
///
/// # Safety
/// `list` must point at ACPI resource descriptors terminated by an end tag.
unsafe fn qword_descriptor(list: *const u8) -> Option<(u64, u64)> {
    const QWORD: u8 = 0x8a;
    // SAFETY: the caller guarantees a terminated list; reads stay inside the current descriptor.
    unsafe {
        let tag = list.read();
        if tag != QWORD {
            return None;
        }
        let min = list.add(14).cast::<u64>().read_unaligned();
        let len = list.add(38).cast::<u64>().read_unaligned();
        (len > 0).then_some((min, len))
    }
}

/// The firmware's view of one found device.
struct Found {
    io: ManuallyDrop<ScopedProtocol<PciIo>>,
    device: PciDevice,
}

/// The devices of [`WANTED`] the firmware has, and their PCI I/O protocols.
pub struct UefiPci {
    found: Vec<Found>,
    devices: Vec<PciDevice>,
    withheld: Cell<bool>,
}

/// Open `PciIo` on `handle` without taking it from any driver; never closed, as some firmware fails
/// the close of a `GetProtocol` open.
fn open(handle: Handle) -> Option<ManuallyDrop<ScopedProtocol<PciIo>>> {
    let params = OpenProtocolParams { handle, agent: boot::image_handle(), controller: None };
    // SAFETY: a shared open; every call through it is a single firmware-serialised protocol call.
    unsafe { boot::open_protocol::<PciIo>(params, OpenProtocolAttributes::GetProtocol) }.ok().map(ManuallyDrop::new)
}

fn describe(io: &PciIo, vendor: u16, device: u16) -> uefi::Result<PciDevice> {
    let location = io.location()?;
    let class_rev = io.config_read(0x08, Width::U32)?;
    let header = io.config_read(0x0e, Width::U8)?;
    let subsystem = io.config_read(0x2c, Width::U32)?;
    let mut bars = [None; 6];
    let mut index = 0_u8;
    while index < 6 {
        let raw = io.config_read(0x10 + u32::from(index) * 4, Width::U32)?;
        let io_bar = raw & 1 == 1;
        let wide = !io_bar && (raw >> 1) & 3 == 2;
        let kind = match (io_bar, wide) {
            (true, _) => BarKind::Io,
            (false, true) => BarKind::Memory64,
            (false, false) => BarKind::Memory32,
        };
        match io.bar_resource(index) {
            Ok(Some((base, size))) => {
                let bar = Bar { base: PhysAddr::new(base), size, kind, prefetchable: !io_bar && raw & 8 != 0 };
                log::info!("{location} BAR {index}: {kind:?} at {base:#x}, {size:#x} bytes (config {raw:#010x})");
                bars[usize::from(index)] = Some(bar);
            },
            Ok(None) => {},
            Err(error) => log::debug!("{location} BAR {index}: no resource ({:?})", error.status()),
        }
        index += if wide { 2 } else { 1 };
    }
    Ok(PciDevice {
        location,
        vendor,
        device,
        subsystem_vendor: subsystem as u16,
        subsystem_device: (subsystem >> 16) as u16,
        class: class_rev >> 8,
        revision: class_rev as u8,
        header_type: header as u8,
        bars,
    })
}

impl UefiPci {
    /// Every PCI I/O handle whose IDs are in [`WANTED`], logged; the others are only counted.
    pub fn discover() -> Self {
        let handles = boot::find_handles::<PciIo>().unwrap_or_else(|error| {
            log::warn!("no PCI I/O handles: {error}");
            Vec::new()
        });
        let mut found = Vec::new();
        for handle in &handles {
            let Some(io) = open(*handle) else { continue };
            let Ok(ids) = io.config_read(0, Width::U32) else { continue };
            let (vendor, device) = (ids as u16, (ids >> 16) as u16);
            let Some(Wanted { name, .. }) = WANTED.iter().find(|w| w.vendor == vendor && w.device == device) else {
                continue;
            };
            match describe(&io, vendor, device) {
                Ok(described) => {
                    log::info!("found {name}: {described}");
                    log_attributes(&io, described.location);
                    found.push(Found { io, device: described });
                },
                Err(error) => log::error!("reading {name} ({vendor:04x}:{device:04x}) failed: {error}"),
            }
        }
        if found.is_empty() {
            let wanted: Vec<String> =
                WANTED.iter().map(|w| format!("{} {:04x}:{:04x}", w.name, w.vendor, w.device)).collect();
            log::info!("no 8265 present: none of {} among {} PCI I/O handles", wanted.join(", "), handles.len());
        }
        let devices = found.iter().map(|f| f.device.clone()).collect();
        Self { found, devices, withheld: Cell::new(false) }
    }

    /// Hand no device to the drivers after all, for a card whose DMA could not be set up. Call before
    /// the C runs.
    pub fn withhold(&self) {
        self.withheld.set(true);
    }

    /// The firmware files the cards found need, for a check before any driver runs.
    pub fn firmware(&self) -> Vec<&'static str> {
        let wanted = |d: &PciDevice| WANTED.iter().find(|w| w.vendor == d.vendor && w.device == d.device);
        self.found.iter().filter_map(|f| wanted(&f.device)).map(|w| w.firmware).collect()
    }

    /// The PCI I/O protocol of device `index`, for mapping DMA memory through it.
    pub fn io(&self, index: usize) -> Option<&PciIo> {
        self.found.get(index).map(|f| &**f.io)
    }

    fn found(&self, index: usize) -> Result<&Found, PciError> {
        self.found.get(index).ok_or(PciError::NoDevice { index })
    }

    fn control(&self, index: usize, what: &'static str, operation: u32, bits: u64) -> Result<(), PciError> {
        let found = self.found(index)?;
        found.io.attributes(operation, bits).map(|_| ()).map_err(|error| PciError::Control {
            location: found.device.location,
            what,
            reason: format!("{:?}", error.status()),
        })
    }
}

/// The config offset of capability `id`, walking the standard list.
fn capability(io: &PciIo, id: u32) -> Option<u32> {
    let status = io.config_read(0x06, Width::U16).ok()?;
    if status & 0x10 == 0 {
        return None;
    }
    let mut at = io.config_read(0x34, Width::U8).ok()? & !3;
    for _ in 0..48 {
        if at < 0x40 {
            return None;
        }
        if io.config_read(at, Width::U8).ok()? == id {
            return Some(at);
        }
        at = io.config_read(at + 1, Width::U8).ok()? & !3;
    }
    None
}

/// The state the firmware left the card in: PCI I/O attributes, command register and power state. A
/// card in D3 answers config cycles but not MMIO, which the driver would see as all-ones registers.
fn log_attributes(io: &PciIo, location: PciAddress) {
    const PM: u32 = 0x01;
    let current = io.attributes(ATTR_GET, 0);
    let supported = io.attributes(ATTR_SUPPORTED, 0);
    let command = io.config_read(0x04, Width::U16);
    let power = capability(io, PM).and_then(|at| io.config_read(at + 4, Width::U16).ok()).map(|pmcsr| pmcsr & 3);
    log::info!(
        "{location}: PCI I/O attributes {:x?}, supported {:x?}; command register {:x?}; power state {}",
        current.map_err(|e| e.status()),
        supported.map_err(|e| e.status()),
        command.map_err(|e| e.status()),
        power.map_or_else(|| String::from("unknown"), |d| format!("D{d}"))
    );
}

impl PciPlatform for UefiPci {
    fn devices(&self) -> &[PciDevice] {
        if self.withheld.get() { &[] } else { &self.devices }
    }

    fn config_read(&self, index: usize, offset: u16, width: Width) -> Result<u32, PciError> {
        let found = self.found(index)?;
        found.io.config_read(u32::from(offset), width).map_err(|error| PciError::Config {
            location: found.device.location,
            offset,
            width,
            reason: format!("{:?}", error.status()),
        })
    }

    fn config_write(&self, index: usize, offset: u16, width: Width, value: u32) -> Result<(), PciError> {
        let found = self.found(index)?;
        found.io.config_write(u32::from(offset), width, value).map_err(|error| PciError::Config {
            location: found.device.location,
            offset,
            width,
            reason: format!("{:?}", error.status()),
        })
    }

    fn enable(&self, index: usize) -> Result<(), PciError> {
        self.control(index, "enabling memory decode", ATTR_ENABLE, ATTR_MEMORY)
    }

    fn set_bus_master(&self, index: usize, on: bool) -> Result<(), PciError> {
        if on {
            self.control(index, "enabling bus mastering", ATTR_ENABLE, ATTR_BUS_MASTER)
        } else {
            self.control(index, "disabling bus mastering", ATTR_DISABLE, ATTR_BUS_MASTER)
        }
    }
}
