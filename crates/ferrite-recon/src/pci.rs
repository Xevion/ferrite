//! PCI identity, class and BAR decoding for the scan apps.

use core::fmt;

use ferrite_xhci::pci::{PowerState, bar_is_64bit};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VendorId(pub u16);

impl VendorId {
    pub const INTEL: Self = Self(0x8086);
}

impl fmt::Display for VendorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04x}", self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceId(pub u16);

impl DeviceId {
    /// Intel Dual Band Wireless-AC 8265, the X1 Gen 6's Wi-Fi card.
    pub const INTEL_8265: Self = Self(0x24fd);
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04x}", self.0)
    }
}

/// Vendor and device from the dword at config offset 0.
#[must_use]
pub const fn split_ids(dword: u32) -> (VendorId, DeviceId) {
    (VendorId(dword as u16), DeviceId((dword >> 16) as u16))
}

/// Base class, subclass and programming interface from the dword at config offset 8.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClassCode {
    pub base: u8,
    pub subclass: u8,
    pub prog_if: u8,
}

impl ClassCode {
    /// Base class of network controllers.
    pub const NETWORK: u8 = 0x02;
    /// Subclass of "other network controller", which is where wireless cards sit.
    pub const NETWORK_OTHER: u8 = 0x80;

    /// The class code and the revision byte below it.
    #[must_use]
    pub const fn from_class_rev(dword: u32) -> (Self, u8) {
        (Self { base: (dword >> 24) as u8, subclass: (dword >> 16) as u8, prog_if: (dword >> 8) as u8 }, dword as u8)
    }

    /// A terse label for the common classes; `None` for the rest.
    #[must_use]
    pub const fn name(self) -> Option<&'static str> {
        Some(match (self.base, self.subclass) {
            (0x01, _) => "storage",
            (Self::NETWORK, Self::NETWORK_OTHER) => "network:other",
            (Self::NETWORK, _) => "network",
            (0x03, _) => "display",
            (0x04, _) => "multimedia",
            (0x06, 0x00) => "host bridge",
            (0x06, 0x01) => "isa bridge",
            (0x06, 0x04) => "pci bridge",
            (0x06, _) => "bridge",
            (0x0c, 0x03) => "usb",
            (0x0c, _) => "serial bus",
            _ => return None,
        })
    }
}

impl fmt::Display for ClassCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02x}:{:02x}:{:02x}", self.base, self.subclass, self.prog_if)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarKind {
    Io,
    Mem32,
    Mem64,
}

impl fmt::Display for BarKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Io => "io",
            Self::Mem32 => "m32",
            Self::Mem64 => "m64",
        })
    }
}

/// BAR0 as the firmware left it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bar {
    pub kind: BarKind,
    pub base: u64,
}

impl Bar {
    /// Decode BAR0's low dword, plus BAR1 as the high half when BAR0 is a 64-bit memory BAR.
    #[must_use]
    pub const fn decode(lo: u32, hi: u32) -> Self {
        if lo & 1 != 0 {
            Self { kind: BarKind::Io, base: (lo & !0x3) as u64 }
        } else if bar_is_64bit(lo) {
            Self { kind: BarKind::Mem64, base: ((hi as u64) << 32) | (lo & !0xf) as u64 }
        } else {
            Self { kind: BarKind::Mem32, base: (lo & !0xf) as u64 }
        }
    }

    /// Whether the base is a memory address a MMIO read can use.
    #[must_use]
    pub const fn is_mmio(self) -> bool {
        !matches!(self.kind, BarKind::Io) && self.base != 0
    }

    #[must_use]
    pub const fn is_64bit(self) -> bool {
        matches!(self.kind, BarKind::Mem64)
    }
}

/// The D-state name from the power-management capability, if the function has one.
#[must_use]
pub const fn power_name(power: Option<PowerState>) -> &'static str {
    match power {
        None => "no PM cap",
        Some(PowerState { state: 0, .. }) => "D0",
        Some(PowerState { state: 1, .. }) => "D1",
        Some(PowerState { state: 2, .. }) => "D2",
        Some(_) => "D3hot",
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::mem32(0xfe20_0000, 0xdead_beef, BarKind::Mem32, 0xfe20_0000)]
    #[case::mem32_prefetch_flags(0xfe20_0008, 0, BarKind::Mem32, 0xfe20_0000)]
    #[case::mem64(0x8000_0004, 0x60, BarKind::Mem64, 0x60_8000_0000)]
    #[case::io(0x0000_e001, 0, BarKind::Io, 0xe000)]
    #[case::unassigned(0, 0, BarKind::Mem32, 0)]
    fn bar_decoding(#[case] lo: u32, #[case] hi: u32, #[case] kind: BarKind, #[case] base: u64) {
        check!(Bar::decode(lo, hi) == Bar { kind, base });
    }

    #[test]
    fn unassigned_and_io_bars_are_not_mmio() {
        check!(!Bar::decode(0, 0).is_mmio());
        check!(!Bar::decode(0xe001, 0).is_mmio());
        check!(Bar::decode(0xfe20_0000, 0).is_mmio());
    }

    #[test]
    fn class_and_ids_split_from_config_dwords() {
        let (class, revision) = ClassCode::from_class_rev(0x0280_0099);
        check!(class == ClassCode { base: 0x02, subclass: 0x80, prog_if: 0x00 });
        check!(revision == 0x99);
        check!(class.name() == Some("network:other"));
        check!(split_ids(0x24fd_8086) == (VendorId::INTEL, DeviceId::INTEL_8265));
    }
}
