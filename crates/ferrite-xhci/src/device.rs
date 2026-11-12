//! An xHCI controller as found in PCI config space, and the capabilities the
//! DbC needs from its MMIO space.

use alloc::vec::Vec;
use core::fmt;
use core::num::NonZeroU8;

use ferrite_hal::{Mmio, MmioExt, PciAddress, PciConfig, PhysAddr, ReadOnly, Reg, RegValue};
use snafu::{OptionExt, ResultExt, ensure};

use crate::error::{Error, InvalidPortSnafu, MmioSnafu, NoControllerSnafu, NoMmioSnafu, NotDecodingSnafu};
use crate::pci::{self, PowerState};
use crate::regs::{ExtCapHeader, ProtocolPorts, cap, xcap};

/// A malformed list could loop forever; real controllers have a handful of entries.
const EXT_CAP_WALK_LIMIT: usize = 64;
/// Supported Protocol major revision for USB 3.
const USB3_MAJOR: u8 = 3;

/// A 1-based root port number inside the controller's `1..=MaxPorts`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PortNumber(NonZeroU8);

impl PortNumber {
    /// Validate `port` against the controller's `MaxPorts`.
    pub fn new(port: u8, max_ports: u8) -> Result<Self, Error> {
        NonZeroU8::new(port).filter(|_| port <= max_ports).map(Self).context(InvalidPortSnafu { port, max_ports })
    }

    #[must_use]
    pub const fn get(self) -> u8 {
        self.0.get()
    }
}

impl fmt::Display for PortNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One entry of the extended capability list.
#[derive(Clone, Copy, Debug)]
pub struct ExtCap {
    /// Address of the capability's first dword.
    pub at: PhysAddr,
    pub header: ExtCapHeader,
}

/// The extended capability list of the controller whose MMIO starts at `mmio`. Reads only.
///
/// The walk ends at the last entry. It also stops, with a warning, at an all-ones
/// header (the function stopped answering) and after 64 entries (a looping list).
pub fn ext_caps<H: Mmio>(hal: &mut H, mmio: PhysAddr) -> Result<Vec<ExtCap>, Error> {
    let mut caps = Vec::new();
    let xecp = read(hal, mmio, cap::HCCPARAMS1)?.xecp();
    if xecp == 0 {
        return Ok(caps);
    }
    let mut at = mmio.offset(u64::from(xecp) * 4);
    for _ in 0..EXT_CAP_WALK_LIMIT {
        let header = read(hal, at, xcap::HEADER)?;
        if header.to_raw() == u32::MAX {
            log::warn!("extended capability at {at} reads as all ones; list cut short");
            return Ok(caps);
        }
        caps.push(ExtCap { at, header });
        if header.next() == 0 {
            return Ok(caps);
        }
        at = at.offset(u64::from(header.next()) * 4);
    }
    log::warn!("extended capability list of {mmio} did not end within {EXT_CAP_WALK_LIMIT} entries");
    Ok(caps)
}

/// Root ports a Supported Protocol capability covers, checked against `MaxPorts`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortRange {
    first: PortNumber,
    last: PortNumber,
}

impl PortRange {
    /// Validate a controller-reported range: `None` when it is empty or starts
    /// outside `1..=max_ports`; a range running past `max_ports` is clamped.
    #[must_use]
    pub(crate) fn validate(ports: ProtocolPorts, max_ports: u8) -> Option<Self> {
        let first = NonZeroU8::new(ports.offset()).filter(|f| f.get() <= max_ports)?;
        let count = ports.count().checked_sub(1)?;
        let last = first.get().saturating_add(count).min(max_ports);
        Some(Self { first: PortNumber(first), last: PortNumber(NonZeroU8::new(last)?) })
    }

    #[must_use]
    pub const fn first(self) -> PortNumber {
        self.first
    }

    #[must_use]
    pub const fn last(self) -> PortNumber {
        self.last
    }

    #[must_use]
    pub fn contains(self, port: u8) -> bool {
        (self.first.get()..=self.last.get()).contains(&port)
    }

    pub fn ports(self) -> impl Iterator<Item = PortNumber> {
        (self.first.0.get()..=self.last.0.get()).filter_map(NonZeroU8::new).map(PortNumber)
    }
}

impl fmt::Display for PortRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.first, self.last)
    }
}

/// An xHCI function as seen in config space, read without side effects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct XhciFunction {
    addr: PciAddress,
    vendor: u16,
    device: u16,
    mmio: PhysAddr,
    command: u16,
    power: Option<PowerState>,
}

impl XhciFunction {
    /// Read `addr`'s config space; `Ok(None)` when no xHCI function is there.
    pub fn identify<H: PciConfig>(hal: &mut H, addr: PciAddress) -> Result<Option<Self>, Error> {
        let id = pci::read32(hal, addr, pci::ID)?;
        if id & 0xffff == 0xffff {
            return Ok(None);
        }
        if pci::read32(hal, addr, pci::CLASS_REV)? >> 8 != pci::XHCI_CLASS {
            return Ok(None);
        }
        let mmio = pci::bar0(hal, addr)?.context(NoMmioSnafu { dev: addr })?;
        let command = pci::read16(hal, addr, pci::COMMAND)?;
        let power = pci::power_state(hal, addr)?;
        Ok(Some(Self { addr, vendor: id as u16, device: (id >> 16) as u16, mmio, command, power }))
    }

    #[must_use]
    pub const fn addr(&self) -> PciAddress {
        self.addr
    }

    #[must_use]
    pub const fn vendor(&self) -> u16 {
        self.vendor
    }

    #[must_use]
    pub const fn device(&self) -> u16 {
        self.device
    }

    #[must_use]
    pub const fn mmio(&self) -> PhysAddr {
        self.mmio
    }

    /// The command register as identified.
    #[must_use]
    pub const fn command(&self) -> u16 {
        self.command
    }

    #[must_use]
    pub const fn power(&self) -> Option<PowerState> {
        self.power
    }

    /// Whether MMIO reads will reach the controller: memory decode on and in D0.
    #[must_use]
    pub fn decoding(&self) -> bool {
        self.command & pci::CMD_MEMORY != 0 && self.power.is_none_or(|p| p.state == 0)
    }
}

/// A controller and the capabilities found in its MMIO space.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct XhciDevice {
    pci: XhciFunction,
    cap_length: u8,
    max_ports: u8,
    dbc_regs: Option<PhysAddr>,
    legacy: Option<PhysAddr>,
    usb3_ports: Vec<PortRange>,
}

impl XhciDevice {
    /// Parse the capability registers and extended capability list. Reads only;
    /// fails with `NotDecoding` if the function cannot answer MMIO yet.
    pub fn probe<H: Mmio>(hal: &mut H, pci: XhciFunction) -> Result<Self, Error> {
        ensure!(pci.decoding(), NotDecodingSnafu { dev: pci.addr, command: pci.command });
        let mmio = pci.mmio;
        let caplength = read(hal, mmio, cap::CAPLENGTH)?;
        ensure!(caplength.to_raw() != u32::MAX, NoControllerSnafu { addr: mmio });
        let max_ports = read(hal, mmio, cap::HCSPARAMS1)?.max_ports();
        let mut dev = Self {
            pci,
            cap_length: caplength.length(),
            max_ports,
            dbc_regs: None,
            legacy: None,
            usb3_ports: Vec::new(),
        };
        for cap in ext_caps(hal, mmio)? {
            dev.note_cap(hal, cap)?;
        }
        Ok(dev)
    }

    fn note_cap<H: Mmio>(&mut self, hal: &mut H, cap: ExtCap) -> Result<(), Error> {
        let ExtCap { at, header } = cap;
        match header.id() {
            xcap::ID_DEBUG => self.dbc_regs = Some(at),
            xcap::ID_LEGACY => self.legacy = Some(at),
            xcap::ID_SUPPORTED_PROTOCOL if header.major_revision() == USB3_MAJOR => {
                let ports = read(hal, at, xcap::PROTOCOL_PORTS)?;
                let (first, count) = (ports.offset(), ports.count());
                match PortRange::validate(ports, self.max_ports) {
                    Some(range) => {
                        if u16::from(first) + u16::from(count) - 1 > u16::from(self.max_ports) {
                            log::warn!(
                                "{}: USB 3 ports {first}+{count} run past MaxPorts {}; clamped to {range}",
                                self.pci.addr,
                                self.max_ports
                            );
                        }
                        self.usb3_ports.push(range);
                    },
                    None => log::warn!(
                        "{}: ignoring USB 3 port range {first}+{count} (MaxPorts {})",
                        self.pci.addr,
                        self.max_ports
                    ),
                }
            },
            _ => {},
        }
        Ok(())
    }

    #[must_use]
    pub const fn pci(&self) -> &XhciFunction {
        &self.pci
    }

    #[must_use]
    pub const fn addr(&self) -> PciAddress {
        self.pci.addr
    }

    #[must_use]
    pub const fn vendor(&self) -> u16 {
        self.pci.vendor
    }

    #[must_use]
    pub const fn mmio(&self) -> PhysAddr {
        self.pci.mmio
    }

    #[must_use]
    pub const fn cap_length(&self) -> u8 {
        self.cap_length
    }

    #[must_use]
    pub const fn max_ports(&self) -> u8 {
        self.max_ports
    }

    /// Base of the DbC register block, if the controller has one.
    #[must_use]
    pub const fn dbc_regs(&self) -> Option<PhysAddr> {
        self.dbc_regs
    }

    /// The USB legacy support capability, if present.
    #[must_use]
    pub const fn legacy(&self) -> Option<PhysAddr> {
        self.legacy
    }

    /// Validated USB 3 root-port ranges.
    #[must_use]
    pub fn usb3_ports(&self) -> &[PortRange] {
        &self.usb3_ports
    }

    /// `port` as a [`PortNumber`] if it lies in one of the USB 3 ranges.
    #[must_use]
    pub fn usb3_port(&self, port: u8) -> Option<PortNumber> {
        NonZeroU8::new(port).filter(|_| self.usb3_ports.iter().any(|r| r.contains(port))).map(PortNumber)
    }

    /// Every root port, USB 2 and USB 3 alike.
    pub fn ports(&self) -> impl Iterator<Item = PortNumber> + use<> {
        (1..=self.max_ports).filter_map(NonZeroU8::new).map(PortNumber)
    }

    /// Base of the operational registers.
    #[must_use]
    pub const fn op_base(&self) -> PhysAddr {
        self.pci.mmio.offset(self.cap_length as u64)
    }
}

impl fmt::Display for XhciDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {:04x}:{:04x}", self.pci.addr, self.pci.vendor, self.pci.device)
    }
}

fn read<H: Mmio, T: RegValue>(hal: &mut H, base: PhysAddr, reg: Reg<T, ReadOnly>) -> Result<T, Error> {
    hal.try_read(base, reg).context(MmioSnafu { op: "read", addr: reg.addr(base) })
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};
    use ferrite_testing::Sim;
    use rstest::rstest;

    use super::*;

    const DEV: PciAddress = PciAddress::new(0, 0x3b, 0, 0);
    const MMIO: u64 = 0xf000_0000;
    const XECP: u64 = 0x800;

    fn sim_with_caps(max_ports: u8, caps: &[(u64, u32, u32)]) -> (Sim, XhciFunction) {
        let sim = Sim::new();
        sim.add_xhci(DEV, MMIO);
        sim.poke(MMIO, 0x0100_0020);
        sim.poke(MMIO + 4, u32::from(max_ports) << 24);
        sim.poke(MMIO + 0x10, ((XECP / 4) as u32) << 16);
        for &(at, header, ports) in caps {
            sim.poke(MMIO + at, header);
            sim.poke(MMIO + at + 8, ports);
        }
        let func = XhciFunction::identify(&mut sim.hal(), DEV).expect("config reads").expect("an xHCI");
        (sim, func)
    }

    fn proto(major: u8, next_dwords: u8) -> u32 {
        (u32::from(major) << 24) | (u32::from(next_dwords) << 8) | u32::from(xcap::ID_SUPPORTED_PROTOCOL)
    }

    #[test]
    fn walk_finds_dbc_legacy_and_usb3_ports() {
        let (sim, func) = sim_with_caps(
            4,
            &[
                (XECP, 0x0000_0401, 0),
                (XECP + 0x10, proto(3, 4), 0x0000_0201),
                (XECP + 0x20, proto(2, 4), 0x0000_0203),
                (XECP + 0x30, 0x0000_000a, 0),
            ],
        );
        let dev = XhciDevice::probe(&mut sim.hal(), func).expect("probe");
        check!(dev.legacy() == Some(PhysAddr::new(MMIO + XECP)));
        check!(dev.dbc_regs() == Some(PhysAddr::new(MMIO + XECP + 0x30)));
        assert!(let [range] = dev.usb3_ports());
        check!((range.first().get(), range.last().get()) == (1, 2));
        check!(dev.op_base() == PhysAddr::new(MMIO + 0x20));
        check!(dev.ports().map(PortNumber::get).collect::<Vec<_>>() == [1, 2, 3, 4], "USB 2 ports too");
    }

    #[rstest]
    #[case::starts_at_zero(0x0000_0200, None)]
    #[case::starts_past_max(0x0000_0105, None)]
    #[case::empty(0x0000_0001, None)]
    #[case::clamped(0x0000_0503, Some((3, 4)))]
    #[case::exact(0x0000_0401, Some((1, 4)))]
    fn usb3_port_ranges_are_validated(#[case] ports: u32, #[case] want: Option<(u8, u8)>) {
        let (sim, func) = sim_with_caps(4, &[(XECP, proto(3, 0), ports)]);
        let dev = XhciDevice::probe(&mut sim.hal(), func).expect("probe");
        let got: Vec<_> = dev.usb3_ports().iter().map(|r| (r.first().get(), r.last().get())).collect();
        check!(got == want.into_iter().collect::<Vec<_>>());
    }

    #[rstest]
    #[case::zero(0, false)]
    #[case::first(1, true)]
    #[case::last(4, true)]
    #[case::past_max_ports(5, false)]
    fn port_numbers_lie_inside_max_ports(#[case] port: u8, #[case] valid: bool) {
        check!(PortNumber::new(port, 4).is_ok() == valid);
    }

    #[test]
    fn walk_stops_on_all_ones() {
        let (sim, func) = sim_with_caps(4, &[(XECP, 0x0000_0401, 0), (XECP + 0x10, u32::MAX, 0)]);
        sim.poke(MMIO + XECP + 0x20, 0x0000_000a);
        let dev = XhciDevice::probe(&mut sim.hal(), func).expect("probe");
        check!(dev.legacy().is_some());
        check!(dev.dbc_regs() == None, "nothing past the all-ones entry is read");
    }

    #[test]
    fn walk_is_capped_on_an_endless_list() {
        let (sim, func) = sim_with_caps(4, &[]);
        for i in 0..2 * EXT_CAP_WALK_LIMIT as u64 {
            sim.poke(MMIO + XECP + 4 * i, 0x0000_0101);
        }
        let reads_before = sim.mmio_reads();
        let dev = XhciDevice::probe(&mut sim.hal(), func).expect("probe terminates");
        check!(dev.legacy().is_some());
        check!(sim.mmio_reads() - reads_before <= 3 + EXT_CAP_WALK_LIMIT);
    }

    #[test]
    fn probe_refuses_a_function_without_decode() {
        let sim = Sim::new();
        sim.add_xhci(DEV, MMIO);
        sim.cfg_poke16(DEV, pci::COMMAND, 0);
        let func = XhciFunction::identify(&mut sim.hal(), DEV).expect("config reads").expect("an xHCI");
        check!(!func.decoding());
        assert!(let Err(Error::NotDecoding { .. }) = XhciDevice::probe(&mut sim.hal(), func));
        check!(sim.writes().is_empty(), "discovery has no side effects");
    }
}
