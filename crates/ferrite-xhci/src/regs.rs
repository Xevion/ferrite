//! Typed register descriptors and values for the xHCI capability, root-port,
//! legacy-support and DbC registers.
//!
//! Registers holding write-one-to-clear fields are [`Rw1c`]: they only accept a
//! [`Rw1cWrite`] from a named helper on the value type, so a value written back
//! as read cannot clear status.

use core::fmt;

use arbitrary_int::{u3, u4, u6, u7};
use bitbybit::{bitenum, bitfield};
use ferrite_hal::{DmaPtrReg, ReadOnly, ReadWrite, Reg, RegValue, Rw1c, Rw1cWrite};

use crate::device::PortNumber;

/// DbC registers, relative to the Debug Capability (extended capability 0x0a).
pub mod dbc {
    use super::{DcCtrl, DcDb, DcSt, Ddi1, Ddi2, DmaPtrReg, ErstSz, PortSc, ReadOnly, ReadWrite, Reg, Rw1c};

    pub const DCDB: Reg<DcDb, ReadWrite> = Reg::new(0x04);
    pub const DCERSTSZ: Reg<ErstSz, ReadWrite> = Reg::new(0x08);
    pub const DCERSTBA: DmaPtrReg = DmaPtrReg::new(0x10);
    pub const DCERDP: DmaPtrReg = DmaPtrReg::new(0x18);
    pub const DCCTRL: Reg<DcCtrl, Rw1c> = Reg::new(0x20);
    pub const DCST: Reg<DcSt, ReadOnly> = Reg::new(0x24);
    pub const DCPORTSC: Reg<PortSc, Rw1c> = Reg::new(0x28);
    pub const DCCP: DmaPtrReg = DmaPtrReg::new(0x30);
    pub const DCDDI1: Reg<Ddi1, ReadWrite> = Reg::new(0x38);
    pub const DCDDI2: Reg<Ddi2, ReadWrite> = Reg::new(0x3c);
}

/// Capability registers, relative to the controller's MMIO base.
pub mod cap {
    use super::{CapLength, HccParams1, HcsParams1, ReadOnly, Reg};

    pub const CAPLENGTH: Reg<CapLength, ReadOnly> = Reg::new(0x00);
    pub const HCSPARAMS1: Reg<HcsParams1, ReadOnly> = Reg::new(0x04);
    pub const HCCPARAMS1: Reg<HccParams1, ReadOnly> = Reg::new(0x10);
}

/// Extended capability registers, relative to the capability's first dword.
pub mod xcap {
    use super::{ExtCapHeader, LegCtlSts, LegSup, ProtocolPorts, ReadOnly, ReadWrite, Reg, Rw1c};

    pub const HEADER: Reg<ExtCapHeader, ReadOnly> = Reg::new(0x00);
    /// Supported Protocol: the root ports it covers.
    pub const PROTOCOL_PORTS: Reg<ProtocolPorts, ReadOnly> = Reg::new(0x08);
    /// USB Legacy Support: BIOS and OS ownership semaphores.
    pub const USBLEGSUP: Reg<LegSup, ReadWrite> = Reg::new(0x00);
    /// USB Legacy Support: SMI enables and SMI status.
    pub const USBLEGCTLSTS: Reg<LegCtlSts, Rw1c> = Reg::new(0x04);

    pub const ID_LEGACY: u8 = 0x01;
    pub const ID_SUPPORTED_PROTOCOL: u8 = 0x02;
    pub const ID_DEBUG: u8 = 0x0a;
}

/// A root port's PORTSC, relative to the operational base.
#[must_use]
pub const fn host_portsc(port: PortNumber) -> Reg<PortSc, Rw1c> {
    const BASE: u64 = 0x400;
    const STRIDE: u64 = 0x10;
    Reg::new(BASE + (port.get() as u64 - 1) * STRIDE)
}

/// Doorbell target for the OUT and IN transfer rings.
pub const DB_OUT: u8 = 0;
pub const DB_IN: u8 = 1;

macro_rules! reg_value {
    ($($ty:ty),* $(,)?) => {$(
        impl RegValue for $ty {
            fn from_raw(raw: u32) -> Self {
                Self::new_with_raw_value(raw)
            }

            fn to_raw(self) -> u32 {
                self.raw_value()
            }
        }
    )*};
}

reg_value!(
    DcCtrl,
    DcSt,
    DcDb,
    ErstSz,
    PortSc,
    Ddi1,
    Ddi2,
    CapLength,
    HcsParams1,
    HccParams1,
    ExtCapHeader,
    ProtocolPorts,
    LegSup,
    LegCtlSts,
);

/// Display for a decoded controller-supplied code: its name, or `unknown(raw)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Code<T, R>(pub Result<T, R>);

impl<T: Copy + Into<&'static str>, R: Copy + fmt::Display> fmt::Display for Code<T, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Ok(v) => f.write_str(v.into()),
            Err(raw) => write!(f, "unknown({raw})"),
        }
    }
}

/// DCCTRL: DbC control.
#[bitfield(u32, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct DcCtrl {
    /// Debug Capability Enable.
    #[bit(31, rw)]
    dce: bool,
    #[bits(24..=30, r)]
    device_address: u7,
    #[bits(16..=23, r)]
    max_burst: u8,
    /// DbC Run Change, write one to clear.
    #[bit(4, rw)]
    drc: bool,
    /// Halt IN TR, write one to set.
    #[bit(3, rw)]
    hit: bool,
    /// Halt OUT TR, write one to set.
    #[bit(2, rw)]
    hot: bool,
    /// Link Status Event Enable.
    #[bit(1, rw)]
    lse: bool,
    /// DbC Run: the far host has configured the device.
    #[bit(0, r)]
    dcr: bool,
}

impl DcCtrl {
    /// Value that sets DCE and LSE, keeps the rest as read, and writes DRC (RW1C)
    /// and HOT and HIT (RW1S) as zero.
    #[must_use]
    pub fn enable(self) -> Rw1cWrite<Self> {
        Rw1cWrite::clearing(self.with_dce(true).with_lse(true).with_drc(false).with_hot(false).with_hit(false))
    }

    /// All zero: clears DCE and leaves DRC (RW1C), HOT and HIT (RW1S) alone.
    #[must_use]
    pub fn disable() -> Rw1cWrite<Self> {
        Rw1cWrite::clearing(Self::DEFAULT)
    }

    /// Value that clears DRC (RW1C) as read, never setting HOT or HIT (RW1S).
    #[must_use]
    pub fn ack_run_change(self) -> Rw1cWrite<Self> {
        Rw1cWrite::clearing(self.with_hot(false).with_hit(false))
    }
}

/// DCST: DbC status.
#[bitfield(u32, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct DcSt {
    /// Root port the DbC is attached to (1-based), 0 when unattached.
    #[bits(24..=31, r)]
    debug_port: u8,
    #[bit(1, r)]
    sbr: bool,
    #[bit(0, r)]
    event_ring_not_empty: bool,
}

/// DCDB: DbC doorbell.
#[bitfield(u32, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct DcDb {
    #[bits(8..=15, rw)]
    target: u8,
}

impl DcDb {
    #[must_use]
    pub const fn ring(target: u8) -> Self {
        Self::DEFAULT.with_target(target)
    }
}

/// DCERSTSZ: event ring segment table size.
#[bitfield(u32, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct ErstSz {
    #[bits(0..=15, rw)]
    entries: u16,
}

/// DCDDI1: vendor in the high half, DbC protocol in the low byte.
#[bitfield(u32, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct Ddi1 {
    #[bits(16..=31, rw)]
    vendor: u16,
    #[bits(0..=7, rw)]
    protocol: u8,
}

/// DCDDI2: device revision in the high half, product in the low half.
#[bitfield(u32, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct Ddi2 {
    #[bits(16..=31, rw)]
    device_rev: u16,
    #[bits(0..=15, rw)]
    product: u16,
}

/// Port link state (PORTSC.PLS). Reserved encodings decode as `Err`.
#[bitenum(u4, exhaustive = false)]
#[derive(Debug, PartialEq, Eq, strum::IntoStaticStr)]
pub enum LinkState {
    U0 = 0,
    U1 = 1,
    U2 = 2,
    U3 = 3,
    Disabled = 4,
    RxDetect = 5,
    Inactive = 6,
    Polling = 7,
    Recovery = 8,
    HotReset = 9,
    Compliance = 10,
    TestMode = 11,
    Resume = 15,
}

/// Port speed (PORTSC.Speed) as a default protocol speed ID. Other IDs decode as `Err`.
#[bitenum(u4, exhaustive = false)]
#[derive(Debug, PartialEq, Eq, strum::IntoStaticStr)]
pub enum PortSpeed {
    #[strum(serialize = "-")]
    Undefined = 0,
    Full = 1,
    Low = 2,
    High = 3,
    Super = 4,
    #[strum(serialize = "Super+")]
    SuperPlus = 5,
}

/// PORTSC layout shared by DCPORTSC and the host root-port registers.
#[bitfield(u32, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct PortSc {
    /// Host ports only: wake on connect, disconnect and over-current enables.
    #[bits(25..=27, rw)]
    wake_enables: u3,
    /// Port Config Error Change (RW1C).
    #[bit(23, rw)]
    cec: bool,
    /// Port Link State Change (RW1C).
    #[bit(22, rw)]
    plc: bool,
    /// Port Reset Change (RW1C).
    #[bit(21, rw)]
    prc: bool,
    /// Connect Status Change (RW1C).
    #[bit(17, rw)]
    csc: bool,
    #[bits(10..=13, r)]
    speed: Option<PortSpeed>,
    /// Host ports only: Port Power.
    #[bit(9, rw)]
    pp: bool,
    #[bits(5..=8, r)]
    link_state: Option<LinkState>,
    /// Port Reset; writing one starts a reset on a host port.
    #[bit(4, rw)]
    pr: bool,
    /// Port Enabled.
    #[bit(1, rw)]
    ped: bool,
    /// Current Connect Status.
    #[bit(0, r)]
    ccs: bool,
}

/// The DCPORTSC change bits: CSC, PRC, PLC and CEC.
const PORTSC_CHANGES: u32 = (1 << 17) | (1 << 21) | (1 << 22) | (1 << 23);
const PORTSC_PED: u32 = 1 << 1;
const PORTSC_PR: u32 = 1 << 4;
/// Host PORTSC bits safe to write back unchanged: PP and the wake enables.
const HOST_PORTSC_PRESERVE: u32 = (1 << 9) | (0x7 << 25);

impl PortSc {
    #[must_use]
    pub const fn changes(self) -> u32 {
        self.raw_value() & PORTSC_CHANGES
    }

    #[must_use]
    pub const fn has_changes(self) -> bool {
        self.changes() != 0
    }

    /// DCPORTSC value that clears the change bits seen (CSC, PRC, PLC and CEC
    /// are RW1C) and keeps PED as read.
    #[must_use]
    pub fn ack_changes(self) -> Rw1cWrite<Self> {
        Rw1cWrite::clearing(Self::new_with_raw_value(self.raw_value() & (PORTSC_CHANGES | PORTSC_PED)))
    }

    /// Host PORTSC value that starts a port reset: PP and wake enables kept, PR
    /// set, and PED and the RW1C bits (CSC, PEC, WRC, OCC, PRC, PLC, CEC) zero.
    #[must_use]
    pub fn host_reset(self) -> Rw1cWrite<Self> {
        Rw1cWrite::clearing(Self::new_with_raw_value((self.raw_value() & HOST_PORTSC_PRESERVE) | PORTSC_PR))
    }

    #[must_use]
    pub fn link(self) -> Code<LinkState, u4> {
        Code(self.link_state())
    }

    #[must_use]
    pub fn speed_code(self) -> Code<PortSpeed, u4> {
        Code(self.speed())
    }
}

/// CAPLENGTH: offset of the operational registers.
#[bitfield(u32, default = 0, debug)]
pub struct CapLength {
    #[bits(0..=7, r)]
    length: u8,
}

/// HCSPARAMS1.
#[bitfield(u32, default = 0, debug)]
pub struct HcsParams1 {
    #[bits(24..=31, r)]
    max_ports: u8,
}

/// HCCPARAMS1.
#[bitfield(u32, default = 0, debug)]
pub struct HccParams1 {
    /// First extended capability, in dwords from the MMIO base; 0 when there is none.
    #[bits(16..=31, r)]
    xecp: u16,
}

/// The first dword of every extended capability.
#[bitfield(u32, default = 0, debug)]
pub struct ExtCapHeader {
    /// Supported Protocol only: major revision (3 for USB 3).
    #[bits(24..=31, r)]
    major_revision: u8,
    /// Dwords to the next capability; 0 ends the list.
    #[bits(8..=15, r)]
    next: u8,
    #[bits(0..=7, r)]
    id: u8,
}

/// Supported Protocol capability, third dword.
#[bitfield(u32, default = 0, debug)]
pub struct ProtocolPorts {
    #[bits(8..=15, r)]
    count: u8,
    /// First root port of the range, 1-based.
    #[bits(0..=7, r)]
    offset: u8,
}

/// USBLEGSUP: ownership semaphores.
#[bitfield(u32, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct LegSup {
    #[bit(24, rw)]
    os_owned: bool,
    #[bit(16, rw)]
    bios_owned: bool,
}

/// USBLEGCTLSTS: SMI enables in the low bits, RW1C SMI status in bits 29 to 31.
#[bitfield(u32, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct LegCtlSts {
    #[bits(29..=31, rw)]
    smi_status: u3,
}

/// Reserved bits, kept as read; every other writable bit is an SMI enable.
const LEGACY_CTL_KEEP: u32 = (0x3 << 1) | (0xff << 5) | (0x7 << 17);
/// SMI status bits, which clear when written as one.
const LEGACY_CTL_EVENTS: u32 = 0x7 << 29;

impl LegCtlSts {
    /// Value that turns every SMI enable off and clears the RW1C SMI status bits.
    #[must_use]
    pub fn disable_smis(self) -> Rw1cWrite<Self> {
        Rw1cWrite::clearing(Self::new_with_raw_value((self.raw_value() & LEGACY_CTL_KEEP) | LEGACY_CTL_EVENTS))
    }
}

/// TRB types the driver produces or consumes. Others decode as `Err`.
#[bitenum(u6, exhaustive = false)]
#[derive(Debug, PartialEq, Eq, strum::IntoStaticStr)]
pub enum TrbType {
    Normal = 1,
    Link = 6,
    TransferEvent = 32,
    CommandCompletion = 33,
    PortStatusChange = 34,
    HostController = 37,
}

impl TrbType {
    /// Name for a raw type field, `unknown(n)` for a type this driver does not know.
    #[must_use]
    pub fn code(raw: u6) -> Code<Self, u6> {
        Code(Self::new_with_raw_value(raw))
    }
}

/// Completion codes in transfer events. Others decode as `Err`.
#[bitenum(u8, exhaustive = false)]
#[derive(Debug, PartialEq, Eq, strum::IntoStaticStr)]
pub enum CompletionCode {
    Success = 1,
    DataBuffer = 2,
    Babble = 3,
    UsbTransaction = 4,
    Trb = 5,
    Stall = 6,
    ShortPacket = 13,
    Stopped = 26,
}

#[cfg(test)]
mod tests {
    use assert2::check;
    use rstest::rstest;

    use super::*;

    /// Status bits that read as one must never be written back as one.
    #[rstest]
    #[case::dcportsc_ack(PortSc::new_with_raw_value(0x00e2_1203).ack_changes().raw(), 0x00e2_0002)]
    #[case::host_reset(PortSc::new_with_raw_value(0x0efe_1203).host_reset().raw(), 0x0e00_0210)]
    #[case::drc_ack(DcCtrl::new_with_raw_value(0x8a00_001f).ack_run_change().raw(), 0x8a00_0013)]
    #[case::enable(DcCtrl::new_with_raw_value(0x0a00_001c).enable().raw(), 0x8a00_0002)]
    #[case::smi_off(LegCtlSts::new_with_raw_value(0xe00f_e01f).disable_smis().raw(), 0xe00e_0006)]
    fn rw1c_write_values(#[case] got: u32, #[case] want: u32) {
        check!(got == want, "{got:#010x} != {want:#010x}");
    }

    #[test]
    fn unknown_codes_name_themselves() {
        let portsc = PortSc::new_with_raw_value(0xc << 5);
        check!(portsc.link_state().is_err());
        check!(std::format!("{}", portsc.link()) == "unknown(12)");
        check!(std::format!("{}", TrbType::code(u6::new(0x3f))) == "unknown(63)");
    }
}
