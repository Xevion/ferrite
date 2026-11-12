//! PCI config and MMIO access through a firmware root bridge, as the `ferrite-hal`
//! [`Mmio`] and [`PciConfig`] backend.

use alloc::vec::Vec;
use core::mem::ManuallyDrop;
use core::ops::RangeInclusive;

use ferrite_hal::{Mmio, PciAddress, PciConfig, PhysAddr, PlatformError};

use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams, ScopedProtocol};
use uefi::proto::pci::PciIoAddress;
use uefi::proto::pci::configuration::ResourceRangeType;
use uefi::proto::pci::root_bridge::PciRootBridgeIo;
use uefi::{Handle, Status};

/// The backend status code for a firmware error.
#[must_use]
pub fn platform_error(e: &uefi::Error) -> PlatformError {
    PlatformError::new(e.status().0)
}

/// Config and MMIO access through one root bridge.
pub struct RootBridge {
    /// Never closed: some firmware does not track a `GetProtocol` open, and closing one there
    /// fails, which `ScopedProtocol`'s drop turns into a panic.
    proto: ManuallyDrop<ScopedProtocol<PciRootBridgeIo>>,
    segment: u16,
}

impl RootBridge {
    /// Open the root bridge protocol on `handle` without taking it from its driver.
    pub fn open(handle: Handle) -> uefi::Result<Self> {
        let params = OpenProtocolParams { handle, agent: boot::image_handle(), controller: None };
        // SAFETY: GetProtocol takes no ownership, so the firmware's own driver keeps
        // using the bridge too. Every access here is a single read or write through
        // the protocol, which the firmware serialises, so the aliasing is benign.
        let proto = unsafe { boot::open_protocol::<PciRootBridgeIo>(params, OpenProtocolAttributes::GetProtocol) }?;
        let segment = u16::try_from(proto.segment_nr()).map_err(|_| uefi::Error::from(Status::UNSUPPORTED))?;
        Ok(Self { proto: ManuallyDrop::new(proto), segment })
    }

    #[must_use]
    pub const fn segment(&self) -> u16 {
        self.segment
    }

    /// Bus numbers decoded below this bridge.
    pub fn bus_ranges(&self) -> uefi::Result<Vec<RangeInclusive<u8>>> {
        Ok(self
            .proto
            .configuration()?
            .iter()
            .filter(|d| d.resource_range_type == ResourceRangeType::Bus)
            .map(|d| clamp_bus(d.address_min)..=clamp_bus(d.address_max))
            .collect())
    }

    /// Every function below this bridge. Walks every bus, so it costs a full scan.
    pub fn enumerate(&mut self) -> uefi::Result<Vec<PciAddress>> {
        let segment = self.segment;
        Ok(self.proto.enumerate()?.iter().filter_map(|a| PciAddress::checked(segment, a.bus, a.dev, a.fun)).collect())
    }

    fn io_addr(&self, dev: PciAddress, offset: u8) -> Result<PciIoAddress, PlatformError> {
        if dev.segment() != self.segment {
            return Err(PlatformError::new(Status::INVALID_PARAMETER.0));
        }
        Ok(PciIoAddress::new(dev.bus(), dev.device(), dev.function()).with_register(offset))
    }
}

fn clamp_bus(v: u64) -> u8 {
    u8::try_from(v).unwrap_or(u8::MAX)
}

impl Mmio for RootBridge {
    fn try_read32(&mut self, addr: PhysAddr) -> Result<u32, PlatformError> {
        self.proto.memory().read_one::<u32>(addr.get()).map_err(|e| platform_error(&e))
    }

    fn try_write32(&mut self, addr: PhysAddr, value: u32) -> Result<(), PlatformError> {
        self.proto.memory().write_one::<u32>(addr.get(), value).map_err(|e| platform_error(&e))
    }
}

impl PciConfig for RootBridge {
    fn cfg_read8(&mut self, dev: PciAddress, offset: u8) -> Result<u8, PlatformError> {
        let at = self.io_addr(dev, offset)?;
        self.proto.pci().read_one::<u8>(at).map_err(|e| platform_error(&e))
    }

    fn cfg_read16(&mut self, dev: PciAddress, offset: u8) -> Result<u16, PlatformError> {
        let at = self.io_addr(dev, offset)?;
        self.proto.pci().read_one::<u16>(at).map_err(|e| platform_error(&e))
    }

    fn cfg_read32(&mut self, dev: PciAddress, offset: u8) -> Result<u32, PlatformError> {
        let at = self.io_addr(dev, offset)?;
        self.proto.pci().read_one::<u32>(at).map_err(|e| platform_error(&e))
    }

    fn cfg_write8(&mut self, dev: PciAddress, offset: u8, value: u8) -> Result<(), PlatformError> {
        let at = self.io_addr(dev, offset)?;
        self.proto.pci().write_one::<u8>(at, value).map_err(|e| platform_error(&e))
    }

    fn cfg_write16(&mut self, dev: PciAddress, offset: u8, value: u16) -> Result<(), PlatformError> {
        let at = self.io_addr(dev, offset)?;
        self.proto.pci().write_one::<u16>(at, value).map_err(|e| platform_error(&e))
    }

    fn cfg_write32(&mut self, dev: PciAddress, offset: u8, value: u32) -> Result<(), PlatformError> {
        let at = self.io_addr(dev, offset)?;
        self.proto.pci().write_one::<u32>(at, value).map_err(|e| platform_error(&e))
    }
}
