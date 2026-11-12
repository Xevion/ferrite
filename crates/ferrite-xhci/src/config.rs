//! DbC configuration and the device identity it presents.

use core::time::Duration;

pub const MANUFACTURER: &str = "ferrite";
pub const PRODUCT: &str = "ferrite DbC";
pub const SERIAL: &str = "0001";

/// Bytes of IN buffer carved as one region, so no TRB buffer can cross a 64 KiB boundary.
pub(crate) const IN_WINDOW: usize = 64 * 1024;

/// Size of each receive TRB on the IN endpoint, the one the debug host writes into.
///
/// A TRB completes when full or on a short packet, so a larger TRB moves more
/// bytes per fetch and per event but holds a host write that ends exactly on a
/// TRB's worth of full packets until more data or a zero-length packet follows.
/// Pick a larger size only against a host that ends every write burst with a zero-length packet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InTrbSize {
    /// One packet per TRB: every packet completes on its own. Right for Linux's `usb_serial` tty writer.
    #[default]
    Kib1,
    Kib4,
    Kib16,
}

impl InTrbSize {
    /// Bytes per TRB, a multiple of the 1 KiB bulk packet.
    #[must_use]
    pub const fn bytes(self) -> usize {
        match self {
            Self::Kib1 => 1024,
            Self::Kib4 => 4 * 1024,
            Self::Kib16 => 16 * 1024,
        }
    }

    /// TRBs kept posted. The IN area is a whole number of 64 KiB windows, and at
    /// 16 KiB it stops at 256 KiB rather than 64 TRBs (1 MiB): that is 16 TRBs,
    /// four host transfers of 64 KiB, far more than a poll interval needs.
    #[must_use]
    pub const fn slots(self) -> usize {
        match self {
            Self::Kib1 => 64,
            Self::Kib4 => 32,
            Self::Kib16 => 16,
        }
    }

    /// 64 KiB regions the buffers occupy.
    #[must_use]
    pub const fn windows(self) -> usize {
        self.slots() * self.bytes() / IN_WINDOW
    }
}

const _: () = {
    let sizes = [InTrbSize::Kib1, InTrbSize::Kib4, InTrbSize::Kib16];
    let mut i = 0;
    while i < sizes.len() {
        let s = sizes[i];
        assert!((s.slots() * s.bytes()).is_multiple_of(IN_WINDOW) && IN_WINDOW.is_multiple_of(s.bytes()));
        assert!(s.slots() <= 64 && s.bytes().is_multiple_of(1024) && s.bytes() <= IN_WINDOW);
        i += 1;
    }
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// How long `wait_configured` waits for the far host.
    pub timeout: Duration,
    /// Re-arm (toggle DCE and reset idle USB 3 ports) this often while waiting.
    pub retry: Duration,
    /// Reset every unconnected USB 3 root port after enabling, as Linux's early driver does on Intel.
    pub port_reset: bool,
    /// Take the controller from the BIOS and mask its SMIs before enabling.
    pub bios_handoff: bool,
    /// Open even though an IOMMU may remap the identity-mapped DMA buffers.
    pub allow_iommu: bool,
    /// First reprogramming attempt after a fault; each failure doubles it.
    pub fault_retry: Duration,
    /// Ceiling for the doubling fault retry interval.
    pub fault_retry_max: Duration,
    /// Size of each IN receive TRB. Fixed at `open`, because it sizes the DMA block.
    pub in_trb: InTrbSize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(10),
            retry: Duration::from_secs(2),
            port_reset: true,
            bios_handoff: true,
            allow_iommu: false,
            fault_retry: Duration::from_millis(500),
            fault_retry_max: Duration::from_secs(16),
            in_trb: InTrbSize::Kib1,
        }
    }
}
