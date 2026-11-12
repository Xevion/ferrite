//! Address types: CPU-physical, device-visible, and device-visible below 4 GiB.

use core::fmt;

/// A CPU-physical address, such as an MMIO register.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PhysAddr(u64);

impl PhysAddr {
    #[must_use]
    pub const fn new(addr: u64) -> Self {
        Self(addr)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    #[must_use]
    pub const fn checked_add(self, bytes: u64) -> Option<Self> {
        match self.0.checked_add(bytes) {
            Some(addr) => Some(Self(addr)),
            None => None,
        }
    }

    /// The address `bytes` further on, for register offsets inside a mapped block.
    ///
    /// # Panics
    /// If the sum overflows, which only a corrupt base address can cause.
    #[must_use]
    pub const fn offset(self, bytes: u64) -> Self {
        match self.checked_add(bytes) {
            Some(addr) => addr,
            None => panic!("physical address overflow"),
        }
    }
}

/// A device-visible DMA address. Identical to the physical address while no
/// IOMMU translates, but kept apart so the two cannot be mixed up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BusAddr(u64);

impl BusAddr {
    #[must_use]
    pub const fn new(addr: u64) -> Self {
        Self(addr)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    #[must_use]
    pub const fn checked_add(self, bytes: u64) -> Option<Self> {
        match self.0.checked_add(bytes) {
            Some(addr) => Some(Self(addr)),
            None => None,
        }
    }
}

/// A device-visible DMA address proven below 4 GiB, so a 64-bit pointer
/// register can take it as two 32-bit writes with a zero high half.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BusAddr32(u32);

impl BusAddr32 {
    /// `None` when `addr` is 4 GiB or above.
    #[must_use]
    pub const fn new(addr: BusAddr) -> Option<Self> {
        if addr.0 <= u32::MAX as u64 { Some(Self(addr.0 as u32)) } else { None }
    }

    #[must_use]
    pub const fn from_u32(addr: u32) -> Self {
        Self(addr)
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    #[must_use]
    pub const fn to_bus(self) -> BusAddr {
        BusAddr(self.0 as u64)
    }

    /// The address `bytes` further on, `None` if that reaches 4 GiB.
    #[must_use]
    pub const fn checked_add(self, bytes: u32) -> Option<Self> {
        match self.0.checked_add(bytes) {
            Some(addr) => Some(Self(addr)),
            None => None,
        }
    }
}

impl From<BusAddr32> for BusAddr {
    fn from(addr: BusAddr32) -> Self {
        addr.to_bus()
    }
}

macro_rules! hex_display {
    ($($ty:ty),*) => {$(
        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{:#x}", self.0)
            }
        }

        impl fmt::LowerHex for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::LowerHex::fmt(&self.0, f)
            }
        }
    )*};
}

hex_display!(PhysAddr, BusAddr, BusAddr32);

/// A PCI function: segment (domain), bus, device and function number.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PciAddress {
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
}

impl PciAddress {
    /// # Panics
    /// If `device` is above 31 or `function` above 7.
    #[must_use]
    pub const fn new(segment: u16, bus: u8, device: u8, function: u8) -> Self {
        assert!(device < 32 && function < 8, "PCI device or function number out of range");
        Self { segment, bus, device, function }
    }

    /// `None` when `device` is above 31 or `function` above 7.
    #[must_use]
    pub const fn checked(segment: u16, bus: u8, device: u8, function: u8) -> Option<Self> {
        if device < 32 && function < 8 { Some(Self { segment, bus, device, function }) } else { None }
    }

    #[must_use]
    pub const fn segment(self) -> u16 {
        self.segment
    }

    #[must_use]
    pub const fn bus(self) -> u8 {
        self.bus
    }

    #[must_use]
    pub const fn device(self) -> u8 {
        self.device
    }

    #[must_use]
    pub const fn function(self) -> u8 {
        self.function
    }
}

impl fmt::Display for PciAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04x}:{:02x}:{:02x}.{}", self.segment, self.bus, self.device, self.function)
    }
}
