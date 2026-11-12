//! DbC configuration and the device identity it presents.

use core::time::Duration;

pub const MANUFACTURER: &str = "ferrite";
pub const PRODUCT: &str = "ferrite DbC";
pub const SERIAL: &str = "0001";

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
        }
    }
}
