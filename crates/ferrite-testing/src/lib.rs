//! Host-only test doubles for the `ferrite-hal` traits.
//!
//! One [`Sim`] owns a sparse MMIO map, PCI config spaces, a DMA arena and a
//! clock. Handles from [`Sim::hal`], [`Sim::clock`] and [`Sim::dma`] share it,
//! so a test can hand them to a driver and keep poking the simulated hardware.
//! A simulated xHCI with a Debug Capability models the DbC registers' access
//! semantics (RW1C, RW1S, DCE side effects) and produces events into the
//! driver's event ring.

pub mod counting;
mod dbc;
mod dma;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::time::Duration;

use ferrite_hal::{Clock, Delay, Instant, Mmio, PciAddress, PciConfig, PhysAddr, PlatformError};

use dbc::DbcModel;
pub use dbc::{CC_SHORT_PACKET, CC_SUCCESS, DBC_DEV, MMIO};
pub use dma::{SimDma, SimRegion};

/// Status code reported for injected failures.
pub const INJECTED_FAILURE: usize = 0xdead;

/// A recorded write, in program order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Write {
    Mmio { addr: u64, value: u32 },
    Config { dev: PciAddress, offset: u8, value: u32 },
}

#[derive(Default)]
struct State {
    mem: BTreeMap<u64, u32>,
    cfg: BTreeMap<PciAddress, [u8; 256]>,
    writes: Vec<Write>,
    mmio_reads: usize,
    fail_mmio: BTreeSet<u64>,
    fail_cfg: BTreeSet<PciAddress>,
    now: Duration,
    xhci_mmio: Option<u64>,
    host_reset_delay: Duration,
    host_resets: BTreeMap<u64, Duration>,
    dbc: Option<DbcModel>,
    dma: dma::Arena,
}

/// The shared simulated platform.
#[derive(Clone, Default)]
pub struct Sim(Rc<RefCell<State>>);

impl Sim {
    #[must_use]
    pub fn new() -> Self {
        let sim = Self::default();
        sim.0.borrow_mut().host_reset_delay = Duration::from_millis(1);
        sim
    }

    /// MMIO and PCI config access.
    #[must_use]
    pub fn hal(&self) -> SimHal {
        SimHal(self.clone())
    }

    /// A clock that only moves when something delays on it.
    #[must_use]
    pub fn clock(&self) -> SimClock {
        SimClock(self.clone())
    }

    #[must_use]
    pub fn dma(&self) -> SimDma {
        SimDma::new(self.clone())
    }

    #[must_use]
    pub fn now(&self) -> Duration {
        self.0.borrow().now
    }

    pub fn advance(&self, by: Duration) {
        self.0.borrow_mut().now += by;
    }

    /// Store a raw MMIO dword, bypassing register semantics and the write log.
    pub fn poke(&self, addr: u64, value: u32) {
        self.0.borrow_mut().mem.insert(addr, value);
    }

    /// The raw MMIO dword as stored.
    #[must_use]
    pub fn peek(&self, addr: u64) -> u32 {
        self.0.borrow().mem.get(&addr).copied().unwrap_or(0)
    }

    #[must_use]
    pub fn writes(&self) -> Vec<Write> {
        self.0.borrow().writes.clone()
    }

    /// MMIO writes to `addr`, oldest first.
    #[must_use]
    pub fn writes_to(&self, addr: u64) -> Vec<u32> {
        self.0
            .borrow()
            .writes
            .iter()
            .filter_map(|w| match *w {
                Write::Mmio { addr: a, value } if a == addr => Some(value),
                _ => None,
            })
            .collect()
    }

    pub fn clear_writes(&self) {
        self.0.borrow_mut().writes.clear();
    }

    #[must_use]
    pub fn mmio_reads(&self) -> usize {
        self.0.borrow().mmio_reads
    }

    /// Make every access to `addr` fail.
    pub fn fail_mmio(&self, addr: u64) {
        self.0.borrow_mut().fail_mmio.insert(addr);
    }

    /// Make every config access to `dev` fail.
    pub fn fail_cfg(&self, dev: PciAddress) {
        self.0.borrow_mut().fail_cfg.insert(dev);
    }

    pub fn cfg_poke8(&self, dev: PciAddress, offset: u8, value: u8) {
        self.cfg_put(dev, offset, &[value]);
    }

    pub fn cfg_poke16(&self, dev: PciAddress, offset: u8, value: u16) {
        self.cfg_put(dev, offset, &value.to_le_bytes());
    }

    pub fn cfg_poke32(&self, dev: PciAddress, offset: u8, value: u32) {
        self.cfg_put(dev, offset, &value.to_le_bytes());
    }

    fn cfg_put(&self, dev: PciAddress, offset: u8, bytes: &[u8]) {
        let mut st = self.0.borrow_mut();
        let space = st.cfg.entry(dev).or_insert([0; 256]);
        let at = usize::from(offset);
        space[at..at + bytes.len()].copy_from_slice(bytes);
    }

    /// An Intel xHCI function at `dev` with a 32-bit BAR0 at `mmio`, decode and
    /// bus mastering on, and no capability list. Root-port PORTSC writes under
    /// `mmio` get reset semantics.
    ///
    /// # Panics
    /// If `mmio` does not fit a 32-bit BAR.
    pub fn add_xhci(&self, dev: PciAddress, mmio: u64) {
        self.cfg_poke32(dev, 0x00, 0x15ec_8086);
        self.cfg_poke32(dev, 0x08, 0x0c03_3000);
        self.cfg_poke32(dev, 0x10, u32::try_from(mmio).unwrap_or_else(|_| panic!("BAR {mmio:#x} is not below 4 GiB")));
        self.cfg_poke16(dev, 0x04, 0x0006);
        self.0.borrow_mut().xhci_mmio = Some(mmio);
    }

    /// How long a host root-port reset takes before PRC reads as set.
    pub fn set_host_reset_delay(&self, delay: Duration) {
        self.0.borrow_mut().host_reset_delay = delay;
    }

    /// Next DMA allocation starts at bus address `base`.
    pub fn set_dma_base(&self, base: u32) {
        self.0.borrow_mut().dma.next = u64::from(base);
    }

    pub fn set_dma_fails(&self, fail: bool) {
        self.0.borrow_mut().dma.fail = fail;
    }

    pub fn set_iommu(&self, active: bool) {
        self.0.borrow_mut().dma.iommu = active;
    }

    /// DMA regions the driver has released.
    #[must_use]
    pub fn dma_freed(&self) -> usize {
        self.0.borrow().dma.regions.iter().filter(|r| r.freed).count()
    }

    /// DMA regions still held.
    #[must_use]
    pub fn dma_live(&self) -> usize {
        self.0.borrow().dma.regions.iter().filter(|r| !r.freed).count()
    }

    /// Simulated device accesses to freed or unmapped DMA memory.
    #[must_use]
    pub fn dma_violations(&self) -> usize {
        self.0.borrow().dma.violations
    }
}

impl State {
    fn read(&mut self, addr: u64) -> Result<u32, PlatformError> {
        self.mmio_reads += 1;
        if self.fail_mmio.contains(&addr) {
            return Err(PlatformError::new(INJECTED_FAILURE));
        }
        if let Some(&done) = self.host_resets.get(&addr)
            && self.now >= done
        {
            self.host_resets.remove(&addr);
            let v = self.mem.get(&addr).copied().unwrap_or(0);
            self.mem.insert(addr, (v & !(1 << 4)) | (1 << 21));
        }
        Ok(self.mem.get(&addr).copied().unwrap_or(0))
    }

    fn write(&mut self, addr: u64, value: u32) -> Result<(), PlatformError> {
        if self.fail_mmio.contains(&addr) {
            return Err(PlatformError::new(INJECTED_FAILURE));
        }
        self.writes.push(Write::Mmio { addr, value });
        if dbc::write(self, addr, value) {
            return Ok(());
        }
        if self.is_host_portsc(addr) {
            self.write_host_portsc(addr, value);
            return Ok(());
        }
        self.mem.insert(addr, value);
        Ok(())
    }

    fn is_host_portsc(&self, addr: u64) -> bool {
        let Some(mmio) = self.xhci_mmio else {
            return false;
        };
        let caplength = u64::from(self.mem.get(&mmio).copied().unwrap_or(0) & 0xff);
        let ports = u64::from(self.mem.get(&(mmio + 4)).copied().unwrap_or(0) >> 24);
        let base = mmio + caplength + 0x400;
        addr >= base && addr < base + ports * 0x10 && (addr - base).is_multiple_of(0x10)
    }

    /// PP and wake enables are RW, PED and bits 17 to 23 are RW1C, PR starts a reset.
    fn write_host_portsc(&mut self, addr: u64, value: u32) {
        const RW: u32 = (1 << 9) | (0x7 << 25);
        const RW1C: u32 = (1 << 1) | (0x7f << 17);
        let old = self.mem.get(&addr).copied().unwrap_or(0);
        let mut new = (old & !RW & !(value & RW1C)) | (value & RW);
        if value & (1 << 4) != 0 {
            new |= 1 << 4;
            self.host_resets.insert(addr, self.now + self.host_reset_delay);
        }
        self.mem.insert(addr, new);
    }

    fn cfg_read(&self, dev: PciAddress, offset: u8, len: usize) -> Result<u32, PlatformError> {
        if self.fail_cfg.contains(&dev) {
            return Err(PlatformError::new(INJECTED_FAILURE));
        }
        let Some(space) = self.cfg.get(&dev) else {
            return Ok(u32::MAX);
        };
        let at = usize::from(offset);
        let mut bytes = [0u8; 4];
        for (i, b) in bytes.iter_mut().enumerate().take(len) {
            *b = space.get(at + i).copied().unwrap_or(0xff);
        }
        Ok(u32::from_le_bytes(bytes))
    }

    fn cfg_write(&mut self, dev: PciAddress, offset: u8, value: u32, len: usize) -> Result<(), PlatformError> {
        if self.fail_cfg.contains(&dev) {
            return Err(PlatformError::new(INJECTED_FAILURE));
        }
        self.writes.push(Write::Config { dev, offset, value });
        let space = self.cfg.entry(dev).or_insert([0; 256]);
        let at = usize::from(offset);
        for (i, b) in value.to_le_bytes().iter().enumerate().take(len) {
            if let Some(slot) = space.get_mut(at + i) {
                *slot = *b;
            }
        }
        Ok(())
    }
}

/// [`Mmio`] and [`PciConfig`] over the shared [`Sim`].
#[derive(Clone)]
pub struct SimHal(Sim);

impl Mmio for SimHal {
    fn try_read32(&mut self, addr: PhysAddr) -> Result<u32, PlatformError> {
        self.0.0.borrow_mut().read(addr.get())
    }

    fn try_write32(&mut self, addr: PhysAddr, value: u32) -> Result<(), PlatformError> {
        self.0.0.borrow_mut().write(addr.get(), value)
    }
}

impl PciConfig for SimHal {
    fn cfg_read8(&mut self, dev: PciAddress, offset: u8) -> Result<u8, PlatformError> {
        self.0.0.borrow().cfg_read(dev, offset, 1).map(|v| v as u8)
    }

    fn cfg_read16(&mut self, dev: PciAddress, offset: u8) -> Result<u16, PlatformError> {
        self.0.0.borrow().cfg_read(dev, offset, 2).map(|v| v as u16)
    }

    fn cfg_read32(&mut self, dev: PciAddress, offset: u8) -> Result<u32, PlatformError> {
        self.0.0.borrow().cfg_read(dev, offset, 4)
    }

    fn cfg_write8(&mut self, dev: PciAddress, offset: u8, value: u8) -> Result<(), PlatformError> {
        self.0.0.borrow_mut().cfg_write(dev, offset, value.into(), 1)
    }

    fn cfg_write16(&mut self, dev: PciAddress, offset: u8, value: u16) -> Result<(), PlatformError> {
        self.0.0.borrow_mut().cfg_write(dev, offset, value.into(), 2)
    }

    fn cfg_write32(&mut self, dev: PciAddress, offset: u8, value: u32) -> Result<(), PlatformError> {
        self.0.0.borrow_mut().cfg_write(dev, offset, value, 4)
    }
}

/// [`Clock`] and [`Delay`] over the shared [`Sim`]: time advances only by delays.
#[derive(Clone)]
pub struct SimClock(Sim);

impl Clock for SimClock {
    fn now(&self) -> Instant {
        Instant::from_epoch(self.0.now())
    }
}

impl Delay for SimClock {
    fn delay(&self, duration: Duration) {
        self.0.advance(duration);
    }
}
