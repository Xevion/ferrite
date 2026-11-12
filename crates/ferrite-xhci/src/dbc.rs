//! The DbC state machine.
//!
//! The driver is polled. `open` programs the DbC and sets DCE; `poll` advances
//! `Armed -> Configured` once the far host enumerates it, drains the event ring
//! and keeps the OUT and IN endpoints fed. Nothing blocks except the explicit
//! `wait_configured`, and every wait has a deadline.

use alloc::string::String;
use core::fmt::{self, Write as _};
use core::sync::atomic::{Ordering, fence};
use core::time::Duration;

use arbitrary_int::u17;
use ferrite_hal::{
    BusAddr32, Clock, Delay, DmaAlloc, DmaPtrReg, DmaRegion, Instant, Mmio, MmioExt, PAGE_SIZE, PageCount, PciConfig,
    PhysAddr, ReadWrite, Readable, Reg, RegValue, Rw1c, Rw1cWrite,
};
use snafu::{OptionExt, ResultExt, ensure};

use crate::config::{Config, MANUFACTURER, PRODUCT, SERIAL};
use crate::device::{PortNumber, XhciDevice};
use crate::dma::{DmaBlock, DmaSlice, boundary_slack};
use crate::endpoint::{Endpoint, HaltChange};
use crate::error::{
    AllocSnafu, DisabledSnafu, DmaError, DmaSnafu, Error, IommuPresentSnafu, MmioSnafu, NoDbcSnafu,
    RegisterTimeoutSnafu,
};
use crate::regs::{CompletionCode, DB_IN, DB_OUT, DcCtrl, ErstSz, PortSc, TrbType, dbc, host_portsc};
use crate::ring::{ByteRing, EventRing, ProducerRing, RING_BYTES, RING_TRBS};
use crate::stats::{RegSnapshot, Stats, Timings};
use crate::trb::{Context, ErstEntry, MAX_PACKET, StringTable, Trb, ddi1, ddi2};
use crate::watch::LinkWatch;
use crate::{host, pci};

/// Software buffer in front of the OUT endpoint.
pub const OUT_BYTES: usize = 16 * 1024;
/// Receive buffers kept posted on the IN endpoint, one packet each.
pub const IN_SLOTS: usize = 8;
const IN_BYTES: usize = IN_SLOTS * MAX_PACKET;

const TABLE_PAGES: PageCount = PageCount::new(1);
const RING_PAGES: PageCount = PageCount::for_bytes(RING_BYTES);
const OUT_PAGES: PageCount = PageCount::for_bytes(OUT_BYTES);
const IN_PAGES: PageCount = PageCount::for_bytes(IN_BYTES);

/// Pages for the table, three rings, the OUT byte ring and the IN buffers, plus
/// what each region may skip to dodge a 64 KiB boundary.
pub const BLOCK_PAGES: PageCount = {
    let regions = [TABLE_PAGES, RING_PAGES, RING_PAGES, RING_PAGES, OUT_PAGES, IN_PAGES];
    let mut total = 0;
    let mut i = 0;
    while i < regions.len() {
        total += regions[i].get() + boundary_slack(regions[i]).get();
        i += 1;
    }
    PageCount::new(total)
};

/// Offsets inside the table page.
const CTX_OFF: usize = 0;
const ERST_OFF: usize = 0x100;
const STR_OFF: usize = 0x200;
const _: () = {
    assert!(CTX_OFF + size_of::<Context>() <= ERST_OFF);
    assert!(ERST_OFF + size_of::<ErstEntry>() <= STR_OFF);
    assert!(STR_OFF + size_of::<StringTable>() <= PAGE_SIZE);
};

const DCE_TIMEOUT: Duration = Duration::from_millis(100);
const REG_POLL: Duration = Duration::from_micros(100);
const CONFIG_POLL: Duration = Duration::from_micros(500);
/// Pause between clearing and re-setting DCE when re-arming.
const DCE_TOGGLE_SETTLE: Duration = Duration::from_millis(1);
/// Read DCST and DCPORTSC at least this often even when nothing signalled a change.
const FULL_READ_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::IntoStaticStr, strum::Display)]
pub enum State {
    Disabled,
    /// DCE is set, waiting for the far host to enumerate the device.
    Armed,
    Configured,
    /// Something failed; [`Dbc::last_fault`] says what. `poll` retries with backoff.
    Faulted,
}

enum Phase {
    Disabled,
    Armed,
    Configured,
    Faulted(Error),
}

/// What [`Dbc::write`] did with the bytes it was given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Written {
    /// Queued for the OUT endpoint.
    pub accepted: usize,
    /// Refused because the buffer was full; counted in `Stats::bytes_refused`.
    pub refused: usize,
}

impl Written {
    #[must_use]
    pub const fn is_complete(self) -> bool {
        self.refused == 0
    }
}

/// The carved DMA regions.
struct Layout {
    table: DmaSlice,
    evt: DmaSlice,
    out_ring: DmaSlice,
    in_ring: DmaSlice,
    out_buf: DmaSlice,
    in_buf: DmaSlice,
}

impl Layout {
    fn carve<R: DmaRegion>(block: &mut DmaBlock<R>) -> Result<Self, DmaError> {
        Ok(Self {
            table: block.carve(TABLE_PAGES.bytes(), PAGE_SIZE)?,
            evt: block.carve(RING_BYTES, PAGE_SIZE)?,
            out_ring: block.carve(RING_BYTES, PAGE_SIZE)?,
            in_ring: block.carve(RING_BYTES, PAGE_SIZE)?,
            out_buf: block.carve(OUT_BYTES, PAGE_SIZE)?,
            in_buf: block.carve(IN_BYTES, PAGE_SIZE)?,
        })
    }
}

impl fmt::Display for Layout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "table {} evt {} out {} in {} outbuf {} inbuf {}",
            self.table.bus(),
            self.evt.bus(),
            self.out_ring.bus(),
            self.in_ring.bus(),
            self.out_buf.bus(),
            self.in_buf.bus()
        )
    }
}

/// The last DCST and DCPORTSC read, and whether they need reading again.
struct Seen {
    ctrl: DcCtrl,
    status: crate::regs::DcSt,
    portsc: PortSc,
    read_at: Option<Instant>,
    port_dirty: bool,
}

/// Fault retry schedule: `None` when the fault must not be retried automatically.
struct Retry {
    at: Option<Instant>,
    delay: Duration,
}

pub struct Dbc<H: Mmio, C: Clock + Delay, R: DmaRegion> {
    hal: H,
    clock: C,
    dev: XhciDevice,
    cfg: Config,
    regs: PhysAddr,
    phase: Phase,
    retry: Retry,
    max_burst: u8,
    table: DmaSlice,
    evt: EventRing,
    out_ep: Endpoint,
    in_ep: Endpoint,
    out_bytes: ByteRing,
    in_buf: DmaSlice,
    in_next_slot: usize,
    stats: Stats,
    timings: Timings,
    watch: LinkWatch,
    seen: Seen,
    /// Our DMA addresses have been written to the controller.
    programmed: bool,
    /// Freed only once DCE reads back clear; leaked otherwise.
    block: Option<DmaBlock<R>>,
}

fn ms(t: Instant) -> u128 {
    t.since_epoch().as_millis()
}

impl<H: Mmio, C: Clock + Delay, R: DmaRegion> Dbc<H, C, R> {
    /// Prepare the PCI function, program the DbC and set DCE. Returns without
    /// waiting for the far host.
    pub fn open<A>(mut hal: H, clock: C, dma: &mut A, dev: &XhciDevice, cfg: &Config) -> Result<Self, Error>
    where
        H: PciConfig,
        A: DmaAlloc<Region = R>,
    {
        let regs = dev.dbc_regs().context(NoDbcSnafu)?;
        if dma.iommu_active() {
            ensure!(cfg.allow_iommu, IommuPresentSnafu);
            log::warn!("IOMMU present; using identity-mapped DMA because Config::allow_iommu is set");
        }
        let prep = pci::prepare(&mut hal, &clock, dev.addr())?;
        log::debug!("pci {dev}: command {:#06x} before, power {:?}", prep.command_before, prep.power_before);

        let region = dma.alloc_dma32(BLOCK_PAGES).context(AllocSnafu { pages: BLOCK_PAGES.get() }).context(DmaSnafu)?;
        let mut block = DmaBlock::new(region).context(DmaSnafu)?;
        let mem = Layout::carve(&mut block).context(DmaSnafu)?;
        log::debug!("dma block {} ({} pages): {mem}", block.bus_addr(), block.pages().get());

        let mut dbc = Self {
            hal,
            clock,
            dev: dev.clone(),
            cfg: *cfg,
            regs,
            phase: Phase::Disabled,
            retry: Retry { at: None, delay: cfg.fault_retry },
            max_burst: 0,
            table: mem.table,
            evt: EventRing::new(mem.evt),
            out_ep: Endpoint::new(ProducerRing::new(mem.out_ring), "OUT", DB_OUT),
            in_ep: Endpoint::new(ProducerRing::new(mem.in_ring), "IN", DB_IN),
            out_bytes: ByteRing::new(mem.out_buf),
            in_buf: mem.in_buf,
            in_next_slot: 0,
            stats: Stats::default(),
            timings: Timings::default(),
            watch: LinkWatch::new(),
            seen: Seen {
                ctrl: DcCtrl::DEFAULT,
                status: crate::regs::DcSt::DEFAULT,
                portsc: PortSc::DEFAULT,
                read_at: None,
                port_dirty: true,
            },
            programmed: false,
            block: Some(block),
        };

        if cfg.bios_handoff
            && let Some(legacy) = dev.legacy()
        {
            let h = host::bios_handoff(&mut dbc.hal, &dbc.clock, legacy)?;
            match (h.bios_owned, h.released) {
                (false, _) => {},
                (true, true) => log::debug!("bios handoff: released after {}ms", h.waited.as_millis()),
                (true, false) => {
                    log::warn!("bios handoff: not released after {}ms, ownership forced", h.waited.as_millis());
                },
            }
        }
        dbc.set_dce(false)?;
        dbc.program()?;
        dbc.enable()?;
        Ok(dbc)
    }

    #[must_use]
    pub const fn state(&self) -> State {
        match self.phase {
            Phase::Disabled => State::Disabled,
            Phase::Armed => State::Armed,
            Phase::Configured => State::Configured,
            Phase::Faulted(_) => State::Faulted,
        }
    }

    /// Why the DbC is `Faulted`; `None` in every other state.
    #[must_use]
    pub const fn last_fault(&self) -> Option<&Error> {
        match &self.phase {
            Phase::Faulted(e) => Some(e),
            _ => None,
        }
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

    #[must_use]
    pub const fn config(&self) -> &Config {
        &self.cfg
    }

    /// Address of DCCTRL, for a platform hook that must clear DCE on its own.
    #[must_use]
    pub const fn dcctrl_addr(&self) -> PhysAddr {
        dbc::DCCTRL.addr(self.regs)
    }

    pub fn snapshot(&mut self) -> RegSnapshot {
        RegSnapshot::read(&mut self.hal, self.regs)
    }

    /// Root port the DbC owns, once a host has connected and the controller says which.
    pub fn debug_port(&mut self) -> Option<PortNumber> {
        let port = self.read(dbc::DCST).debug_port();
        self.dev.usb3_port(port)
    }

    /// The host controller's own PORTSC for a root port.
    pub fn host_portsc(&mut self, port: PortNumber) -> PortSc {
        self.hal.read(self.dev.op_base(), host_portsc(port))
    }

    /// Bytes accepted but not yet confirmed sent.
    #[must_use]
    pub const fn buffered(&self) -> usize {
        self.out_bytes.buffered()
    }

    /// Queue bytes for the OUT endpoint without blocking. Bytes that do not fit
    /// are refused, not queued.
    pub fn write(&mut self, data: &[u8]) -> Written {
        // After `release` the buffer memory may be gone.
        let accepted = if self.block.is_some() { self.out_bytes.write(data) } else { 0 };
        let refused = data.len() - accepted;
        self.stats.bytes_queued += accepted as u64;
        self.stats.bytes_refused += refused as u64;
        self.service();
        Written { accepted, refused }
    }

    /// Clear DCE and wait for it to read back clear. On failure the DbC is
    /// `Faulted` and not retried automatically.
    pub fn disable(&mut self) -> Result<(), Error> {
        match self.set_dce(false) {
            Ok(()) => {
                self.phase = Phase::Disabled;
                Ok(())
            },
            Err(e) => {
                self.fault(e.clone(), false);
                Err(e)
            },
        }
    }

    /// Disable and release the DMA memory, or leak it if DCE will not clear.
    pub fn close(mut self) -> Result<(), Error> {
        self.release()
    }

    /// Poll until the far host configures the device or `Config::timeout`
    /// passes, re-arming every `Config::retry`. Returns the time waited.
    pub fn wait_configured(&mut self) -> Result<Duration, Error> {
        let start = self.clock.now();
        let mut last_kick = start;
        loop {
            self.poll(|_| {});
            match &self.phase {
                Phase::Configured => return Ok(self.clock.elapsed(start)),
                Phase::Disabled => return DisabledSnafu.fail(),
                Phase::Faulted(e) => return Err(e.clone()),
                Phase::Armed => {},
            }
            let now = self.clock.now();
            if now - start >= self.cfg.timeout {
                let snap = self.snapshot();
                log::warn!("t={}ms not configured by the host after {}ms:\n{snap}", ms(now), (now - start).as_millis());
                return RegisterTimeoutSnafu {
                    op: "wait for host configuration",
                    reg: "DCCTRL",
                    value: snap.ctrl.raw_value(),
                }
                .fail();
            }
            if now - last_kick >= self.cfg.retry {
                last_kick = now;
                self.kick();
            }
            self.clock.delay(CONFIG_POLL);
        }
    }

    /// Advance the state machine, drain events, deliver received bytes to `rx`
    /// and keep both endpoints fed. From `Faulted`, retries on schedule.
    pub fn poll(&mut self, mut rx: impl FnMut(&[u8])) {
        match self.phase {
            Phase::Disabled => return,
            Phase::Faulted(_) => {
                self.try_recover();
                return;
            },
            Phase::Armed | Phase::Configured => {},
        }
        let now = self.clock.now();
        let ctrl = self.read(dbc::DCCTRL);
        let refresh = self.seen.port_dirty
            || ctrl != self.seen.ctrl
            || self.seen.read_at.is_none_or(|t| now - t >= FULL_READ_INTERVAL);
        self.seen.ctrl = ctrl;
        if refresh {
            self.seen.status = self.read(dbc::DCST);
            self.seen.portsc = self.read(dbc::DCPORTSC);
            self.seen.read_at = Some(now);
            self.seen.port_dirty = false;
        }
        let portsc = self.seen.portsc;
        self.watch.observe(now, ctrl, self.seen.status, portsc);

        if ctrl.raw_value() == u32::MAX || !ctrl.dce() {
            self.fault(Error::DceLost { ctrl: ctrl.raw_value() }, true);
            return;
        }
        let configured = matches!(self.phase, Phase::Configured);
        if refresh {
            if portsc.ccs() && self.timings.connected.is_none() {
                self.timings.connected = Some(now);
            }
            if portsc.has_changes() {
                self.stats.port_changes += 1;
                self.hal.write_rw1c(self.regs, dbc::DCPORTSC, portsc.ack_changes());
                if portsc.prc() && configured {
                    self.rearm("debug port reset");
                    return;
                }
            }
            if configured && !portsc.ped() && !portsc.ccs() {
                self.rearm("cable unplugged");
                return;
            }
        }

        if ctrl.drc() {
            self.hal.write_rw1c(self.regs, dbc::DCCTRL, ctrl.ack_run_change());
        }
        match (configured, ctrl.dcr()) {
            (false, true) => {
                self.timings.configured.get_or_insert(now);
                self.phase = Phase::Configured;
                log::info!("t={}ms configured by host (device address {})", ms(now), ctrl.device_address());
            },
            (true, false) => {
                self.rearm("DCR dropped");
                return;
            },
            _ => {},
        }

        if matches!(self.phase, Phase::Configured) {
            let out = self.out_ep.track_halt(ctrl.hot());
            self.on_halt_change(self.out_ep.name, out);
            let inn = self.in_ep.track_halt(ctrl.hit());
            self.on_halt_change(self.in_ep.name, inn);
        }

        self.drain_events(&mut rx);
        self.service();
    }

    fn read<T: RegValue, A: Readable>(&mut self, reg: Reg<T, A>) -> T {
        self.hal.read(self.regs, reg)
    }

    fn try_read<T: RegValue, A: Readable>(&mut self, reg: Reg<T, A>) -> Result<T, Error> {
        self.hal.try_read(self.regs, reg).context(MmioSnafu { op: "read", addr: reg.addr(self.regs) })
    }

    fn try_write<T: RegValue>(&mut self, reg: Reg<T, ReadWrite>, value: T) -> Result<(), Error> {
        self.hal.try_write(self.regs, reg, value).context(MmioSnafu { op: "write", addr: reg.addr(self.regs) })
    }

    fn try_write_rw1c<T: RegValue>(&mut self, reg: Reg<T, Rw1c>, value: Rw1cWrite<T>) -> Result<(), Error> {
        self.hal.try_write_rw1c(self.regs, reg, value).context(MmioSnafu { op: "write", addr: reg.addr(self.regs) })
    }

    fn try_write_ptr(&mut self, reg: DmaPtrReg, value: BusAddr32) -> Result<(), Error> {
        self.hal.try_write_dma_ptr(self.regs, reg, value).context(MmioSnafu { op: "write", addr: reg.addr(self.regs) })
    }

    /// Set (with link status events on) or clear DCE and wait for the readback.
    fn set_dce(&mut self, on: bool) -> Result<(), Error> {
        let value = if on { self.try_read(dbc::DCCTRL)?.enable() } else { DcCtrl::disable() };
        self.try_write_rw1c(dbc::DCCTRL, value)?;
        let t0 = self.clock.now();
        loop {
            let ctrl = self.try_read(dbc::DCCTRL)?;
            if ctrl.dce() == on {
                return Ok(());
            }
            if self.clock.elapsed(t0) >= DCE_TIMEOUT {
                return RegisterTimeoutSnafu {
                    op: if on { "set DCE" } else { "clear DCE" },
                    reg: "DCCTRL",
                    value: ctrl.raw_value(),
                }
                .fail();
            }
            self.clock.delay(REG_POLL);
        }
    }

    /// Reset rings and rewrite every DbC register and context. DCE must be clear.
    /// Unacknowledged OUT bytes are kept and sent again.
    fn program(&mut self) -> Result<(), Error> {
        self.programmed = true;
        self.evt.reset();
        self.out_ep.reset();
        self.in_ep.reset();
        self.out_bytes.rewind();
        self.in_next_slot = 0;

        self.max_burst = self.try_read(dbc::DCCTRL)?.max_burst();
        self.table.zero();
        self.write_context();
        self.table.write(ERST_OFF, ErstEntry::new(self.evt.dma(), RING_TRBS as u32));

        self.try_write(dbc::DCERSTSZ, ErstSz::DEFAULT.with_entries(1))?;
        self.try_write_ptr(dbc::DCERSTBA, self.table.bus_at(ERST_OFF))?;
        self.try_write_ptr(dbc::DCERDP, self.evt.dequeue_dma())?;
        self.try_write_ptr(dbc::DCCP, self.table.bus_at(CTX_OFF))?;
        self.try_write(dbc::DCDDI1, ddi1())?;
        self.try_write(dbc::DCDDI2, ddi2())?;
        fence(Ordering::SeqCst);
        Ok(())
    }

    fn write_context(&self) {
        let strings = StringTable::new(MANUFACTURER, PRODUCT, SERIAL);
        let strings_dma = self.table.bus_at(STR_OFF);
        let ctx = Context::new(&strings, strings_dma, self.max_burst, self.out_ep.ring.dma(), self.in_ep.ring.dma());
        self.table.write(CTX_OFF, ctx);
        self.table.write(STR_OFF, strings);
    }

    /// Set DCE, optionally reset the idle USB 3 ports, and enter `Armed`.
    fn enable(&mut self) -> Result<(), Error> {
        self.set_dce(true)?;
        let now = self.clock.now();
        self.timings.dce_ready.get_or_insert(now);
        self.phase = Phase::Armed;
        self.seen.port_dirty = true;
        let snap = self.snapshot();
        log::debug!("t={}ms DCE set, max burst {}\n{snap}", ms(now), self.max_burst);
        if self.cfg.port_reset {
            match host::reset_debug_ports(&mut self.hal, &self.clock, &self.dev) {
                Ok(resets) if resets.is_empty() => {},
                Ok(resets) => {
                    let line = resets.iter().fold(String::new(), |mut line, r| {
                        // Writing to a String cannot fail.
                        let _ =
                            write!(line, " {}:{:#010x}->{:#010x}", r.port, r.before.raw_value(), r.after.raw_value());
                        line
                    });
                    log::debug!("port reset:{line}");
                },
                Err(e) => log::warn!("port reset failed, continuing without it: {e}"),
            }
        }
        Ok(())
    }

    /// Toggle DCE and reprogram everything.
    fn reprogram(&mut self) -> Result<(), Error> {
        self.set_dce(false)?;
        self.clock.delay(DCE_TOGGLE_SETTLE);
        self.program()?;
        self.watch.forget();
        self.enable()
    }

    /// The recovery Linux uses when enumeration stalls.
    fn kick(&mut self) {
        self.stats.kicks += 1;
        log::info!("t={}ms still not configured, re-arming (kick {})", ms(self.clock.now()), self.stats.kicks);
        if let Err(e) = self.reprogram() {
            self.fault(e, true);
        }
    }

    fn fault(&mut self, error: Error, retry: bool) {
        let now = self.clock.now();
        self.stats.faults += 1;
        if retry {
            self.retry.at = now.checked_add(self.retry.delay);
            log::warn!("t={}ms DbC faulted: {error}; retrying in {}ms", ms(now), self.retry.delay.as_millis());
            self.retry.delay = (self.retry.delay * 2).min(self.cfg.fault_retry_max);
        } else {
            self.retry.at = None;
            log::error!("t={}ms DbC faulted: {error}", ms(now));
        }
        self.phase = Phase::Faulted(error);
    }

    fn try_recover(&mut self) {
        let now = self.clock.now();
        if self.retry.at.is_none_or(|at| now < at) {
            return;
        }
        match self.reprogram() {
            Ok(()) => {
                self.stats.recoveries += 1;
                self.retry.delay = self.cfg.fault_retry;
                log::info!("t={}ms recovered from fault, re-armed", ms(self.clock.now()));
            },
            Err(e) => self.fault(e, true),
        }
    }

    /// Back to `Armed` after the host dropped the device: restart both rings
    /// from their initial state, requeue unacknowledged OUT bytes and rewrite
    /// the endpoint contexts.
    fn rearm(&mut self, why: &str) {
        self.stats.rearms += 1;
        self.out_ep.reset();
        self.in_ep.reset();
        self.out_bytes.rewind();
        self.in_next_slot = 0;
        self.write_context();
        self.phase = Phase::Armed;
        self.seen.port_dirty = true;
        log::info!("t={}ms re-armed: {why}; {} bytes requeued", ms(self.clock.now()), self.out_bytes.buffered());
    }

    fn on_halt_change(&mut self, name: &str, change: Option<HaltChange>) {
        match change {
            None => {},
            Some(HaltChange::Halted) => {
                self.stats.halts += 1;
                log::warn!("t={}ms {name} endpoint halted", ms(self.clock.now()));
            },
            Some(HaltChange::Cleared(doorbell)) => {
                log::info!("t={}ms {name} endpoint halt cleared", ms(self.clock.now()));
                if let Some(db) = doorbell {
                    self.hal.write(self.regs, dbc::DCDB, db);
                }
            },
        }
    }

    fn drain_events(&mut self, rx: &mut impl FnMut(&[u8])) {
        let mut any = false;
        while let Some(event) = self.evt.pop() {
            any = true;
            self.stats.events += 1;
            let control = event.control();
            self.stats.event_types[usize::from(control.trb_type_raw().value())] += 1;
            match control.trb_type() {
                Ok(TrbType::TransferEvent) => self.on_transfer(&event, rx),
                Ok(TrbType::PortStatusChange) => self.seen.port_dirty = true,
                _ => {},
            }
        }
        if any {
            self.hal.write_dma_ptr(self.regs, dbc::DCERDP, self.evt.dequeue_dma());
        }
    }

    fn on_transfer(&mut self, event: &Trb, rx: &mut impl FnMut(&[u8])) {
        let status = event.event_status();
        let ok = matches!(status.completion_code(), Ok(CompletionCode::Success | CompletionCode::ShortPacket));
        let ptr = event.pointer();
        if !ok {
            self.stats.transfer_errors += 1;
            self.stats.last_error_cc = status.completion_code_raw();
            log::warn!(
                "t={}ms transfer event error cc={} ep_id={} ptr={ptr:#x}",
                ms(self.clock.now()),
                crate::regs::Code(status.completion_code()),
                event.control().endpoint_id(),
            );
        }

        let mut acked = 0usize;
        if self.out_ep.ring.complete_to(ptr, |len| acked += usize::from(len)).is_some() {
            self.stats.out_done += 1;
            self.out_bytes.ack(acked);
            if ok {
                self.stats.bytes_sent += acked as u64;
            } else {
                self.stats.bytes_dropped += acked as u64;
            }
            return;
        }

        let mut slot = None;
        if self.in_ep.ring.complete_to(ptr, |s| slot = Some(s)).is_some() {
            self.stats.in_done += 1;
            if ok && let Some(slot) = slot {
                let got = MAX_PACKET.saturating_sub(status.residual().value() as usize);
                let mut packet = [0u8; MAX_PACKET];
                self.in_buf.read_into(usize::from(slot) * MAX_PACKET, &mut packet[..got]);
                self.stats.bytes_received += got as u64;
                rx(&packet[..got]);
            }
            return;
        }

        self.stats.unknown_transfer_events += 1;
        log::debug!("t={}ms transfer event for {ptr:#x} matches no in-flight TRB", ms(self.clock.now()));
    }

    /// Turn buffered bytes into OUT TRBs and keep IN buffers posted.
    fn service(&mut self) {
        if !matches!(self.phase, Phase::Configured) {
            return;
        }
        let mut ring_out = false;
        let mut ring_in = false;

        if !self.out_ep.halted() {
            while self.out_ep.ring.free() > 0 {
                let Some((addr, len)) = self.out_bytes.next_chunk(MAX_PACKET) else {
                    break;
                };
                if !self.out_ep.ring.push(Trb::normal(addr, u17::new(len as u32)), len as u16) {
                    break;
                }
                self.out_bytes.mark_sent(len);
                ring_out = true;
            }
        }
        if !self.in_ep.halted() {
            while self.in_ep.ring.pending() < IN_SLOTS && self.in_ep.ring.free() > 0 {
                let slot = self.in_next_slot;
                let addr = self.in_buf.bus_at(slot * MAX_PACKET);
                if !self.in_ep.ring.push(Trb::normal(addr, u17::new(MAX_PACKET as u32)), slot as u16) {
                    break;
                }
                self.in_next_slot = (slot + 1) % IN_SLOTS;
                ring_in = true;
            }
        }

        fence(Ordering::SeqCst);
        if ring_out {
            self.hal.write(self.regs, dbc::DCDB, self.out_ep.doorbell());
        }
        if ring_in {
            self.hal.write(self.regs, dbc::DCDB, self.in_ep.doorbell());
        }
    }

    /// Clear DCE, then free the DMA block; if DCE will not clear after our
    /// addresses were programmed, leak the block instead. The DbC is unusable
    /// afterwards, and calling this again does nothing.
    pub fn release(&mut self) -> Result<(), Error> {
        let Some(block) = self.block.take() else {
            return Ok(());
        };
        match self.set_dce(false) {
            Ok(()) => {
                self.phase = Phase::Disabled;
                drop(block);
                Ok(())
            },
            Err(e) if self.programmed => {
                log::error!(
                    "DCE will not clear ({e}); leaking {} DMA pages at {} that the controller may still use",
                    block.pages().get(),
                    block.bus_addr()
                );
                block.into_leaked();
                self.fault(e.clone(), false);
                Err(e)
            },
            Err(e) => {
                log::warn!("DCE will not clear ({e}); our DMA block was never programmed, so it is freed");
                drop(block);
                self.fault(e.clone(), false);
                Err(e)
            },
        }
    }
}

impl<H: Mmio, C: Clock + Delay, R: DmaRegion> Drop for Dbc<H, C, R> {
    fn drop(&mut self) {
        // `release` logs its own failure, and there is nobody left to report it to.
        let _ = self.release();
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;
    use ferrite_testing::Sim;

    use super::*;

    #[test]
    fn layout_fits_block_at_every_offset_in_a_64k_window() {
        let sim = Sim::new();
        for page in 0..16u32 {
            sim.set_dma_base(0x10_0000 + page * PAGE_SIZE as u32);
            let mut block = DmaBlock::new(sim.dma().alloc_dma32(BLOCK_PAGES).expect("sim allocation")).expect("low");
            check!(Layout::carve(&mut block).is_ok(), "base page {page}");
        }
    }
}
