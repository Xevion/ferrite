//! xHCI Debug Capability driver: a USB 3 debug serial device presented by one
//! root port of the host controller we run on.
//!
//! The driver is polled. `open` programs the DbC and sets DCE; `poll` advances
//! `Armed -> Configured` once the far host enumerates it, drains the event ring
//! and keeps the OUT and IN endpoints fed. Nothing here blocks except the
//! explicit `wait_configured`, and every wait has a deadline.

pub mod clock;
pub mod discover;
pub mod dma;
pub mod regs;
pub mod ring;
pub mod trb;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use core::sync::atomic::{Ordering, fence};
use core::time::Duration;

use uefi::boot;

use self::clock::Clock;
use self::discover::{RootBridge, XhciDevice};
use self::dma::{DmaBlock, DmaError, DmaSlice, PAGE};
use self::regs::{
    CTRL_DCE, CTRL_DCR, CTRL_DRC, CTRL_HIT, CTRL_HOT, CTRL_LSE, DB_IN, DB_OUT, DCCP, DCCTRL, DCDB,
    DCDDI1, DCDDI2, DCERDP, DCERSTBA, DCERSTSZ, DCPORTSC, DCST, HOST_PORT_STRIDE, HOST_PORTSC_BASE,
    LEGACY_BIOS_OWNED, LEGACY_CTL_EVENTS, LEGACY_CTL_KEEP, LEGACY_OS_OWNED, PORTSC_CCS,
    PORTSC_CHANGES, PORTSC_PED, PORTSC_PR, PORTSC_PRC, PORTSC_PRESERVE,
};
use self::ring::{ByteRing, EventRing, ProducerRing, RING_TRBS};
use self::trb::{
    CC_SHORT_PACKET, CC_SUCCESS, Context, ErstEntry, MAX_PACKET, StringTable,
    TRB_TYPE_PORT_STATUS_EVENT, TRB_TYPE_TRANSFER_EVENT, Trb,
};

pub const MANUFACTURER: &str = "ferrite";
pub const PRODUCT: &str = "ferrite DbC";
pub const SERIAL: &str = "0001";

/// Software buffer in front of the OUT endpoint.
const OUT_BYTES: usize = 16 * 1024;
/// Receive buffers kept posted on the IN endpoint, one packet each.
const IN_SLOTS: usize = 8;
const PACKET: usize = MAX_PACKET as usize;

const CTX_OFF: usize = 0;
const ERST_OFF: usize = 0x100;
const STR_OFF: usize = 0x200;

/// Pages for the table, three rings, the OUT byte ring and the IN buffers, plus
/// slack so every region can dodge a 64 KiB boundary.
const BLOCK_PAGES: usize = 1 + 3 + OUT_BYTES / PAGE + IN_SLOTS * PACKET / PAGE + 16;

const DCE_TIMEOUT_MS: u32 = 100;
const INTEL_VENDOR: u16 = 0x8086;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Disabled,
    /// DCE is set, waiting for the far host to enumerate the device.
    Armed,
    Configured,
    Faulted,
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Disabled => "Disabled",
            Self::Armed => "Armed",
            Self::Configured => "Configured",
            Self::Faulted => "Faulted",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DbcError {
    NoDbc,
    NoBridge,
    Dma(DmaError),
    /// DCE did not clear within the deadline.
    DisableTimeout {
        ctrl: u32,
    },
    /// DCE did not read back as set.
    EnableTimeout {
        ctrl: u32,
    },
    /// The far host never configured the device.
    ConfigTimeout {
        ctrl: u32,
        status: u32,
        portsc: u32,
    },
    Faulted,
}

impl fmt::Display for DbcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDbc => f.write_str("controller has no debug capability"),
            Self::NoBridge => f.write_str("cannot open the PCI root bridge"),
            Self::Dma(e) => write!(f, "DMA allocation failed: {e:?}"),
            Self::DisableTimeout { ctrl } => write!(f, "DCE would not clear (DCCTRL={ctrl:#010x})"),
            Self::EnableTimeout { ctrl } => write!(f, "DCE would not set (DCCTRL={ctrl:#010x})"),
            Self::ConfigTimeout {
                ctrl,
                status,
                portsc,
            } => write!(
                f,
                "not configured by host (DCCTRL={ctrl:#010x} DCST={status:#010x} DCPORTSC={portsc:#010x})"
            ),
            Self::Faulted => f.write_str("DbC faulted"),
        }
    }
}

impl From<DmaError> for DbcError {
    fn from(e: DmaError) -> Self {
        Self::Dma(e)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// How long `wait_configured` waits for the far host.
    pub timeout_ms: u32,
    /// Re-arm (toggle DCE and reset idle USB 3 ports) this often while waiting.
    pub retry_ms: u32,
    /// Reset every unconnected USB 3 root port after enabling, as Linux's early driver does on Intel.
    pub port_reset: bool,
    /// Take the controller from the BIOS and mask its SMIs before enabling.
    pub bios_handoff: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            timeout_ms: 10_000,
            retry_ms: 3_000,
            port_reset: true,
            bios_handoff: true,
        }
    }
}

/// First time (ms since the clock started) each milestone was observed.
#[derive(Clone, Copy, Debug, Default)]
pub struct Timings {
    pub dce_ready_ms: Option<u64>,
    pub connected_ms: Option<u64>,
    pub configured_ms: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct Stats {
    pub events: u32,
    /// Event TRB counts indexed by TRB type.
    pub event_types: [u32; 64],
    pub out_done: u32,
    pub in_done: u32,
    pub unknown_transfer_events: u32,
    pub transfer_errors: u32,
    pub last_error_cc: u8,
    pub bytes_queued: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    /// Bytes refused because the software buffer was full, or discarded on re-arm.
    pub bytes_dropped: u64,
    pub port_changes: u32,
    pub halts: u32,
    pub rearms: u32,
    pub kicks: u32,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            events: 0,
            event_types: [0; 64],
            out_done: 0,
            in_done: 0,
            unknown_transfer_events: 0,
            transfer_errors: 0,
            last_error_cc: 0,
            bytes_queued: 0,
            bytes_sent: 0,
            bytes_received: 0,
            bytes_dropped: 0,
            port_changes: 0,
            halts: 0,
            rearms: 0,
            kicks: 0,
        }
    }
}

/// Raw DbC registers at one instant.
#[derive(Clone, Copy, Debug)]
pub struct RegSnapshot {
    pub ctrl: u32,
    pub status: u32,
    pub portsc: u32,
    pub erstsz: u32,
    pub erstba: u64,
    pub erdp: u64,
    pub dccp: u64,
    pub ddi1: u32,
    pub ddi2: u32,
}

impl RegSnapshot {
    fn read(rb: &mut RootBridge, regs: u64) -> Self {
        Self {
            ctrl: rb.read32(regs + DCCTRL),
            status: rb.read32(regs + DCST),
            portsc: rb.read32(regs + DCPORTSC),
            erstsz: rb.read32(regs + DCERSTSZ),
            erstba: rb.read64(regs + DCERSTBA),
            erdp: rb.read64(regs + DCERDP),
            dccp: rb.read64(regs + DCCP),
            ddi1: rb.read32(regs + DCDDI1),
            ddi2: rb.read32(regs + DCDDI2),
        }
    }

    /// Human-readable lines for a report.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let c = self.ctrl;
        let p = self.portsc;
        let flag = |v: u32, bit: u32| u8::from(v & bit != 0);
        alloc::vec![
            format!(
                "DCCTRL={c:#010x} DCE={} DCR={} LSE={} HOT={} HIT={} DRC={} maxburst={} devaddr={}",
                flag(c, CTRL_DCE),
                flag(c, CTRL_DCR),
                flag(c, CTRL_LSE),
                flag(c, CTRL_HOT),
                flag(c, CTRL_HIT),
                flag(c, CTRL_DRC),
                regs::ctrl_max_burst(c),
                regs::ctrl_dev_addr(c),
            ),
            format!(
                "DCST={:#010x} debug_port={}",
                self.status,
                regs::status_debug_port(self.status)
            ),
            format!(
                "DCPORTSC={p:#010x} CCS={} PED={} PR={} link={} speed={} changes={:#x}",
                flag(p, PORTSC_CCS),
                flag(p, PORTSC_PED),
                flag(p, PORTSC_PR),
                regs::link_state_name((p >> 5) & 0xf),
                regs::speed_name((p >> 10) & 0xf),
                p & PORTSC_CHANGES,
            ),
            format!(
                "ERSTSZ={} ERSTBA={:#x} ERDP={:#x} DCCP={:#x} DDI1={:#010x} DDI2={:#010x}",
                self.erstsz, self.erstba, self.erdp, self.dccp, self.ddi1, self.ddi2
            ),
        ]
    }
}

/// Read a controller's DbC registers without touching them.
#[must_use]
pub fn snapshot_device(dev: &XhciDevice) -> Option<RegSnapshot> {
    let regs = dev.dbc?;
    let mut rb = RootBridge::open(dev.bridge)?;
    Some(RegSnapshot::read(&mut rb, regs))
}

pub struct Dbc {
    bridge: RootBridge,
    dev: XhciDevice,
    cfg: Config,
    clock: Clock,
    regs: u64,
    state: State,
    max_burst: u8,
    table: DmaSlice,
    evt: EventRing,
    out: ProducerRing,
    inn: ProducerRing,
    out_bytes: ByteRing,
    in_buf: DmaSlice,
    in_next_slot: usize,
    out_halted: bool,
    in_halted: bool,
    stats: Stats,
    timings: Timings,
    last_seen: (u32, u32, u32),
    watch_budget: u32,
    // Dropped last: the DbC is disabled in `Drop::drop` before the memory goes.
    _block: DmaBlock,
}

impl Dbc {
    /// Program the DbC and set DCE. Returns without waiting for the far host.
    pub fn open(
        dev: &XhciDevice,
        cfg: &Config,
        clock: Clock,
        note: &mut dyn FnMut(&str),
    ) -> Result<Self, DbcError> {
        let regs = dev.dbc.ok_or(DbcError::NoDbc)?;
        let mut bridge = RootBridge::open(dev.bridge).ok_or(DbcError::NoBridge)?;
        prepare_pci(&mut bridge, dev, note);

        let mut block = DmaBlock::alloc(BLOCK_PAGES)?;
        let table = block.carve(PAGE, PAGE)?;
        let evt_mem = block.carve(RING_TRBS * size_of::<Trb>(), PAGE)?;
        let out_mem = block.carve(RING_TRBS * size_of::<Trb>(), PAGE)?;
        let in_mem = block.carve(RING_TRBS * size_of::<Trb>(), PAGE)?;
        let out_buf = block.carve(OUT_BYTES, PAGE)?;
        let in_buf = block.carve(IN_SLOTS * PACKET, PAGE)?;
        note(&format!(
            "dma block {:#x} ({} pages): table {:#x} evt {:#x} out {:#x} in {:#x} outbuf {:#x} inbuf {:#x}",
            block.phys(),
            BLOCK_PAGES,
            table.phys(),
            evt_mem.phys(),
            out_mem.phys(),
            in_mem.phys(),
            out_buf.phys(),
            in_buf.phys(),
        ));

        let mut dbc = Self {
            bridge,
            dev: dev.clone(),
            cfg: *cfg,
            clock,
            regs,
            state: State::Disabled,
            max_burst: 0,
            table,
            evt: EventRing::new(evt_mem.ptr().cast(), evt_mem.phys()),
            out: ProducerRing::new(out_mem.ptr().cast(), out_mem.phys()),
            inn: ProducerRing::new(in_mem.ptr().cast(), in_mem.phys()),
            out_bytes: ByteRing::new(out_buf.ptr(), out_buf.phys(), OUT_BYTES),
            in_buf,
            in_next_slot: 0,
            out_halted: false,
            in_halted: false,
            stats: Stats::default(),
            timings: Timings::default(),
            last_seen: (0, 0, 0),
            watch_budget: 200,
            _block: block,
        };

        if cfg.bios_handoff {
            dbc.bios_handoff(note);
        }
        if !dbc.set_dce(false) {
            return Err(DbcError::DisableTimeout {
                ctrl: dbc.rd(DCCTRL),
            });
        }
        dbc.program();
        dbc.enable(note)?;
        Ok(dbc)
    }

    #[must_use]
    pub const fn state(&self) -> State {
        self.state
    }

    #[must_use]
    pub const fn stats(&self) -> &Stats {
        &self.stats
    }

    #[must_use]
    pub const fn timings(&self) -> Timings {
        self.timings
    }

    #[must_use]
    pub const fn device(&self) -> &XhciDevice {
        &self.dev
    }

    pub fn snapshot(&mut self) -> RegSnapshot {
        RegSnapshot::read(&mut self.bridge, self.regs)
    }

    /// Root port the DbC owns (1-based), or 0 if the controller has not said.
    pub fn debug_port(&mut self) -> u8 {
        regs::status_debug_port(self.rd(DCST))
    }

    /// The host controller's own PORTSC for a 1-based root port.
    pub fn host_portsc(&mut self, port: u8) -> u32 {
        let addr = self.host_portsc_addr(port);
        self.bridge.read32(addr)
    }

    /// Bytes accepted but not yet confirmed sent.
    #[must_use]
    pub const fn buffered(&self) -> usize {
        self.out_bytes.buffered()
    }

    /// Queue bytes for the OUT endpoint without blocking. Returns how many were
    /// taken; the rest are counted as dropped.
    pub fn write(&mut self, data: &[u8]) -> usize {
        let n = self.out_bytes.write(data);
        self.stats.bytes_queued += n as u64;
        self.stats.bytes_dropped += (data.len() - n) as u64;
        self.service();
        n
    }

    /// Clear DCE and wait for it to read back clear.
    pub fn disable(&mut self) -> bool {
        let ok = self.set_dce(false);
        self.state = State::Disabled;
        ok
    }

    /// Poll until the far host configures the device or `cfg.timeout_ms` passes,
    /// re-arming every `cfg.retry_ms`. Returns the elapsed milliseconds.
    pub fn wait_configured(&mut self, note: &mut dyn FnMut(&str)) -> Result<u64, DbcError> {
        let start = self.clock.ms();
        let mut last_kick = start;
        loop {
            self.poll(&mut |_| {}, note);
            let now = self.clock.ms();
            match self.state {
                State::Configured => return Ok(now - start),
                State::Faulted | State::Disabled => return Err(DbcError::Faulted),
                State::Armed => {}
            }
            if now - start >= u64::from(self.cfg.timeout_ms) {
                let snap = self.snapshot();
                return Err(DbcError::ConfigTimeout {
                    ctrl: snap.ctrl,
                    status: snap.status,
                    portsc: snap.portsc,
                });
            }
            if now - last_kick >= u64::from(self.cfg.retry_ms) {
                last_kick = now;
                self.kick(note);
            }
            boot::stall(Duration::from_micros(500));
        }
    }

    /// Advance the state machine, drain events, deliver received bytes to `rx`
    /// and keep both endpoints fed.
    pub fn poll(&mut self, rx: &mut dyn FnMut(&[u8]), note: &mut dyn FnMut(&str)) {
        if matches!(self.state, State::Disabled | State::Faulted) {
            return;
        }
        let ctrl = self.rd(DCCTRL);
        let status = self.rd(DCST);
        let portsc = self.rd(DCPORTSC);
        self.watch(ctrl, status, portsc, note);

        if ctrl == u32::MAX || ctrl & CTRL_DCE == 0 {
            self.state = State::Faulted;
            note(&format!(
                "t={}ms DCE lost (DCCTRL={ctrl:#010x}); controller reset?",
                self.clock.ms()
            ));
            return;
        }

        if portsc & PORTSC_CCS != 0 && self.timings.connected_ms.is_none() {
            self.timings.connected_ms = Some(self.clock.ms());
        }
        if portsc & PORTSC_CHANGES != 0 {
            self.stats.port_changes += 1;
            self.bridge_write(DCPORTSC, portsc);
            if portsc & PORTSC_PRC != 0 && self.state == State::Configured {
                self.rearm("debug port reset", note);
            }
        }
        if self.state == State::Configured && portsc & (PORTSC_PED | PORTSC_CCS) == 0 {
            self.rearm("cable unplugged", note);
        }

        if ctrl & CTRL_DRC != 0 {
            self.bridge_write(DCCTRL, ctrl & !(CTRL_HOT | CTRL_HIT));
        }
        let running = ctrl & CTRL_DCR != 0;
        match self.state {
            State::Armed if running => {
                let t = self.clock.ms();
                self.timings.configured_ms.get_or_insert(t);
                self.state = State::Configured;
                note(&format!("t={t}ms configured by host"));
            }
            State::Configured if !running => self.rearm("DCR dropped", note),
            _ => {}
        }

        if self.state == State::Configured {
            self.track_halt(ctrl & CTRL_HOT != 0, true, note);
            self.track_halt(ctrl & CTRL_HIT != 0, false, note);
        }

        self.drain_events(rx, note);
        self.service();
    }

    fn rd(&mut self, off: u64) -> u32 {
        self.bridge.read32(self.regs + off)
    }

    fn bridge_write(&mut self, off: u64, val: u32) {
        self.bridge.write32(self.regs + off, val);
    }

    fn host_portsc_addr(&self, port: u8) -> u64 {
        self.dev.op_base() + HOST_PORTSC_BASE + u64::from(port.saturating_sub(1)) * HOST_PORT_STRIDE
    }

    fn wait_reg(&mut self, off: u64, mask: u32, want: u32, timeout_ms: u32) -> bool {
        let t0 = self.clock.ms();
        loop {
            if self.rd(off) & mask == want {
                return true;
            }
            if self.clock.ms() - t0 >= u64::from(timeout_ms) {
                return false;
            }
            boot::stall(Duration::from_micros(100));
        }
    }

    /// Set (with link status events on) or clear DCE and wait for the readback.
    fn set_dce(&mut self, on: bool) -> bool {
        if on {
            let ctrl = self.rd(DCCTRL);
            self.bridge_write(DCCTRL, ctrl | CTRL_DCE | CTRL_LSE);
            self.wait_reg(DCCTRL, CTRL_DCE, CTRL_DCE, DCE_TIMEOUT_MS)
        } else {
            self.bridge_write(DCCTRL, 0);
            self.wait_reg(DCCTRL, CTRL_DCE, 0, DCE_TIMEOUT_MS)
        }
    }

    /// Reset rings and rewrite every DbC register and context. DCE must be clear.
    fn program(&mut self) {
        self.evt.reset();
        self.out.reset();
        self.inn.reset();
        self.out_bytes.reset();
        self.in_next_slot = 0;
        self.out_halted = false;
        self.in_halted = false;

        self.max_burst = regs::ctrl_max_burst(self.rd(DCCTRL));
        self.table.zero();
        self.write_context();
        let erst = ErstEntry {
            base: self.evt.dma(),
            size: RING_TRBS as u32,
            reserved: 0,
        };
        self.table.write(ERST_OFF, erst);

        let table = self.table.phys();
        self.bridge_write(DCERSTSZ, 1);
        self.bridge
            .write64(self.regs + DCERSTBA, table + ERST_OFF as u64);
        self.bridge
            .write64(self.regs + DCERDP, self.evt.dequeue_dma());
        self.bridge
            .write64(self.regs + DCCP, table + CTX_OFF as u64);
        self.bridge_write(DCDDI1, trb::ddi1());
        self.bridge_write(DCDDI2, trb::ddi2());
        fence(Ordering::SeqCst);
    }

    fn write_context(&mut self) {
        let strings = StringTable::new(MANUFACTURER, PRODUCT, SERIAL);
        let strings_dma = self.table.phys() + STR_OFF as u64;
        let ctx = Context::new(
            &strings,
            strings_dma,
            self.max_burst,
            self.out.dma(),
            self.inn.dma(),
        );
        self.table.write(CTX_OFF, ctx);
        self.table.write(STR_OFF, strings);
    }

    /// Set DCE, optionally reset the idle USB 3 ports, and enter `Armed`.
    fn enable(&mut self, note: &mut dyn FnMut(&str)) -> Result<(), DbcError> {
        if !self.set_dce(true) {
            return Err(DbcError::EnableTimeout {
                ctrl: self.rd(DCCTRL),
            });
        }
        let t = self.clock.ms();
        self.timings.dce_ready_ms.get_or_insert(t);
        self.state = State::Armed;
        let snap = self.snapshot();
        note(&format!("t={t}ms DCE set, max burst {}", self.max_burst));
        for line in snap.lines() {
            note(&format!("  {line}"));
        }
        if self.cfg.port_reset {
            self.reset_debug_ports(note);
        }
        Ok(())
    }

    /// Toggle DCE and reprogram everything: the recovery Linux uses when
    /// enumeration stalls or the controller was reset under it.
    fn kick(&mut self, note: &mut dyn FnMut(&str)) {
        self.stats.kicks += 1;
        note(&format!(
            "t={}ms still not configured, re-arming (kick {})",
            self.clock.ms(),
            self.stats.kicks
        ));
        self.set_dce(false);
        boot::stall(Duration::from_millis(1));
        self.program();
        self.last_seen = (0, 0, 0);
        if let Err(e) = self.enable(note) {
            self.state = State::Faulted;
            note(&format!("re-arm failed: {e}"));
        }
    }

    /// Reset every unconnected root port in the controller's USB 3 ranges. With
    /// the debug port reset here, Intel parts retrain a link that came up
    /// before DCE was set.
    fn reset_debug_ports(&mut self, note: &mut dyn FnMut(&str)) {
        if self.dev.vendor != INTEL_VENDOR {
            return;
        }
        let debug_port = self.debug_port();
        let before = if debug_port != 0 {
            self.host_portsc(debug_port)
        } else {
            0
        };
        let mut reset = Vec::new();
        for (first, count) in self.dev.usb3_ports.clone() {
            for port in first..first.saturating_add(count) {
                let addr = self.host_portsc_addr(port);
                let val = self.bridge.read32(addr);
                if val != u32::MAX && val & PORTSC_CCS == 0 {
                    self.bridge
                        .write32(addr, (val & PORTSC_PRESERVE) | PORTSC_PR);
                    reset.push(port);
                }
            }
        }
        boot::stall(Duration::from_millis(20));
        let after = if debug_port != 0 {
            self.host_portsc(debug_port)
        } else {
            0
        };
        note(&format!(
            "port reset: ports {reset:?}; debug port {debug_port} host PORTSC {before:#010x} -> {after:#010x}"
        ));
    }

    fn bios_handoff(&mut self, note: &mut dyn FnMut(&str)) {
        let Some(legacy) = self.dev.legacy else {
            return;
        };
        let val = self.bridge.read32(legacy);
        if val & LEGACY_BIOS_OWNED != 0 {
            self.bridge.write32(legacy, val | LEGACY_OS_OWNED);
            let t0 = self.clock.ms();
            let mut released = false;
            while self.clock.ms() - t0 < 1000 {
                if self.bridge.read32(legacy) & LEGACY_BIOS_OWNED == 0 {
                    released = true;
                    break;
                }
                boot::stall(Duration::from_micros(500));
            }
            if !released {
                let v = self.bridge.read32(legacy);
                self.bridge.write32(legacy, v & !LEGACY_BIOS_OWNED);
            }
            note(&format!(
                "bios handoff: legacy={val:#010x} released={released} after {}ms",
                self.clock.ms() - t0
            ));
        }
        let ctl = self.bridge.read32(legacy + 4);
        self.bridge
            .write32(legacy + 4, (ctl & LEGACY_CTL_KEEP) | LEGACY_CTL_EVENTS);
    }

    /// Log register changes with a timestamp so a report shows the link timeline.
    fn watch(&mut self, ctrl: u32, status: u32, portsc: u32, note: &mut dyn FnMut(&str)) {
        if self.last_seen == (ctrl, status, portsc) {
            return;
        }
        self.last_seen = (ctrl, status, portsc);
        if self.watch_budget == 0 {
            return;
        }
        self.watch_budget -= 1;
        note(&format!(
            "t={}ms DCCTRL={ctrl:#010x} DCST={status:#010x} DCPORTSC={portsc:#010x} ({} {} {})",
            self.clock.ms(),
            if portsc & PORTSC_CCS != 0 {
                "connected"
            } else {
                "disconnected"
            },
            regs::link_state_name((portsc >> 5) & 0xf),
            regs::speed_name((portsc >> 10) & 0xf),
        ));
    }

    /// Back to `Armed` after the host dropped the device: restart both rings
    /// from their initial state and rewrite the endpoint contexts.
    fn rearm(&mut self, why: &str, note: &mut dyn FnMut(&str)) {
        self.stats.rearms += 1;
        self.stats.bytes_dropped += self.out_bytes.buffered() as u64;
        self.out.reset();
        self.inn.reset();
        self.out_bytes.reset();
        self.in_next_slot = 0;
        self.out_halted = false;
        self.in_halted = false;
        self.write_context();
        self.state = State::Armed;
        note(&format!("t={}ms re-armed: {why}", self.clock.ms()));
    }

    fn track_halt(&mut self, halted: bool, out: bool, note: &mut dyn FnMut(&str)) {
        let (flag, name, target, pending) = if out {
            (&mut self.out_halted, "OUT", DB_OUT, self.out.pending())
        } else {
            (&mut self.in_halted, "IN", DB_IN, self.inn.pending())
        };
        if *flag == halted {
            return;
        }
        *flag = halted;
        if halted {
            self.stats.halts += 1;
        }
        note(&format!(
            "t={}ms {name} endpoint {}",
            self.clock.ms(),
            if halted { "halted" } else { "halt cleared" }
        ));
        if !halted && pending > 0 {
            self.bridge_write(DCDB, target << 8);
        }
    }

    fn drain_events(&mut self, rx: &mut dyn FnMut(&[u8]), note: &mut dyn FnMut(&str)) {
        let mut any = false;
        while let Some(event) = self.evt.pop() {
            any = true;
            self.stats.events += 1;
            self.stats.event_types[(event.trb_type() & 63) as usize] += 1;
            match event.trb_type() {
                TRB_TYPE_TRANSFER_EVENT => self.on_transfer(&event, rx, note),
                TRB_TYPE_PORT_STATUS_EVENT => {}
                _ => {}
            }
        }
        if any {
            let erdp = self.evt.dequeue_dma();
            self.bridge.write64(self.regs + DCERDP, erdp);
        }
    }

    fn on_transfer(&mut self, event: &Trb, rx: &mut dyn FnMut(&[u8]), note: &mut dyn FnMut(&str)) {
        let cc = event.completion_code();
        let ok = cc == CC_SUCCESS || cc == CC_SHORT_PACKET;
        if !ok {
            self.stats.transfer_errors += 1;
            self.stats.last_error_cc = cc;
            note(&format!(
                "t={}ms transfer event error cc={cc} ep_id={} ptr={:#x}",
                self.clock.ms(),
                event.endpoint_id(),
                event.pointer()
            ));
        }
        let ptr = event.pointer();
        if self.out.contains(ptr) {
            self.stats.out_done += 1;
            if let Some(len) = self.out.complete() {
                self.out_bytes.ack(usize::from(len));
                if ok {
                    self.stats.bytes_sent += u64::from(len);
                }
            }
        } else if self.inn.contains(ptr) {
            self.stats.in_done += 1;
            if let Some(slot) = self.inn.complete()
                && ok
            {
                let got = PACKET.saturating_sub(event.residual() as usize);
                let mut packet = [0u8; PACKET];
                self.in_buf
                    .read_into(usize::from(slot) * PACKET, &mut packet[..got]);
                self.stats.bytes_received += got as u64;
                rx(&packet[..got]);
            }
        } else {
            self.stats.unknown_transfer_events += 1;
        }
    }

    /// Turn buffered bytes into OUT TRBs and keep IN buffers posted.
    fn service(&mut self) {
        if self.state != State::Configured {
            return;
        }
        let mut ring_out = false;
        let mut ring_in = false;

        if !self.out_halted {
            while self.out.free() > 0 {
                let Some((addr, len)) = self.out_bytes.next_chunk(PACKET) else {
                    break;
                };
                if !self.out.push(Trb::normal(addr, len as u32), len as u16) {
                    break;
                }
                self.out_bytes.mark_sent(len);
                ring_out = true;
            }
        }
        if !self.in_halted {
            while self.inn.pending() < IN_SLOTS && self.inn.free() > 0 {
                let slot = self.in_next_slot;
                let addr = self.in_buf.phys() + (slot * PACKET) as u64;
                if !self.inn.push(Trb::normal(addr, MAX_PACKET), slot as u16) {
                    break;
                }
                self.in_next_slot = (slot + 1) % IN_SLOTS;
                ring_in = true;
            }
        }

        fence(Ordering::SeqCst);
        if ring_out {
            self.bridge_write(DCDB, DB_OUT << 8);
        }
        if ring_in {
            self.bridge_write(DCDB, DB_IN << 8);
        }
    }
}

impl Drop for Dbc {
    fn drop(&mut self) {
        // The controller must stop DMAing before the buffers are freed.
        self.set_dce(false);
    }
}

/// Bring the device to D0 with memory decode and bus mastering on.
fn prepare_pci(rb: &mut RootBridge, dev: &XhciDevice, note: &mut dyn FnMut(&str)) {
    let cmd = rb.enable_dma(dev.addr).unwrap_or(0);
    let power = rb.power_state(dev.addr);
    note(&format!(
        "pci {dev}: command {cmd:#06x} (before) power {power:?}"
    ));
    if let Some((pmcsr_off, state)) = power
        && state != 0
        && let Some(pmcsr) = rb.cfg_read16(dev.addr, pmcsr_off)
    {
        rb.cfg_write16(dev.addr, pmcsr_off, pmcsr & !0x3);
        boot::stall(Duration::from_millis(10));
        note("moved controller to D0");
    }
}
