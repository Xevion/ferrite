//! Driver errors.

use ferrite_hal::{BusAddr, PciAddress, PhysAddr, PlatformError};
use snafu::Snafu;

#[derive(Clone, Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum DmaError {
    #[snafu(display("allocating {pages} DMA pages below 4 GiB failed"))]
    Alloc { pages: usize, source: PlatformError },

    #[snafu(display("DMA block at {addr} ({len} bytes) is not below 4 GiB"))]
    Above4G { addr: BusAddr, len: usize },

    #[snafu(display("DMA block has no room for {len} bytes at alignment {align}"))]
    OutOfSpace { len: usize, align: usize },
}

#[derive(Clone, Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum Error {
    #[snafu(display("controller has no debug capability"))]
    NoDbc,

    #[snafu(display("an IOMMU may remap DMA and buffers are identity mapped (Config::allow_iommu overrides)"))]
    IommuPresent,

    #[snafu(display("{dev}: memory decode is off or the function is not in D0 (command {command:#06x})"))]
    NotDecoding { dev: PciAddress, command: u16 },

    #[snafu(display("{dev}: BAR0 is not a memory BAR"))]
    NoMmio { dev: PciAddress },

    #[snafu(display("controller at {addr} reads as all ones"))]
    NoController { addr: PhysAddr },

    #[snafu(display("MMIO {op} at {addr} failed"))]
    Mmio { op: &'static str, addr: PhysAddr, source: PlatformError },

    #[snafu(display("PCI config {op} {dev} offset {offset:#04x} failed"))]
    PciConfig { op: &'static str, dev: PciAddress, offset: u8, source: PlatformError },

    #[snafu(display("DMA setup failed"))]
    Dma { source: DmaError },

    #[snafu(display("{op}: {reg} still {value:#010x} at the deadline"))]
    RegisterTimeout { op: &'static str, reg: &'static str, value: u32 },

    #[snafu(display("DCE dropped (DCCTRL={ctrl:#010x}); controller reset or unreachable"))]
    DceLost { ctrl: u32 },

    #[snafu(display("root port {port} is outside 1..={max_ports}"))]
    InvalidPort { port: u8, max_ports: u8 },

    #[snafu(display("the DbC is disabled"))]
    Disabled,
}
