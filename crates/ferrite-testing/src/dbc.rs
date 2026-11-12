//! A simulated xHCI with a Debug Capability.
//!
//! Register offsets and bit positions are restated here rather than shared
//! with the driver, so the model checks the driver instead of echoing it.

use ferrite_hal::PciAddress;

use crate::{Sim, State};

pub const DBC_DEV: PciAddress = PciAddress::new(0, 0x3b, 0, 0);
pub const MMIO: u64 = 0xf000_0000;
const XECP: u64 = 0x800;
const LEGACY: u64 = MMIO + XECP;
const DBC: u64 = MMIO + XECP + 0x30;
const MAX_PORTS: u32 = 4;
/// Root port the simulated debug host is cabled to.
const DEBUG_PORT: u32 = 1;

const DCERSTBA: u64 = 0x10;
const DCERDP: u64 = 0x18;
const DCCTRL: u64 = 0x20;
const DCST: u64 = 0x24;
const DCPORTSC: u64 = 0x28;
const DCCP: u64 = 0x30;

const CTRL_DCR: u32 = 1 << 0;
const CTRL_LSE: u32 = 1 << 1;
const CTRL_HALTS: u32 = (1 << 2) | (1 << 3);
const CTRL_DRC: u32 = 1 << 4;
const CTRL_DEV_ADDR: u32 = 0x7f << 24;
const CTRL_DCE: u32 = 1 << 31;
const CTRL_MAX_BURST: u32 = 0x0f << 16;

const PORT_CCS: u32 = 1 << 0;
const PORT_PED: u32 = 1 << 1;
const PORT_SUPERSPEED: u32 = 4 << 10;
const PORT_CSC: u32 = 1 << 17;
const PORT_PRC: u32 = 1 << 21;
const PORT_CHANGES: u32 = (1 << 17) | (1 << 21) | (1 << 22) | (1 << 23);

const TRB_LINK: u32 = 6;
const TRB_TRANSFER_EVENT: u32 = 32;
const TRB_PORT_STATUS_EVENT: u32 = 34;
const EP_OUT: u32 = 2;
const EP_IN: u32 = 3;
pub const CC_SUCCESS: u8 = 1;
pub const CC_SHORT_PACKET: u8 = 13;

/// Where the simulated DbC is in a transfer ring.
#[derive(Clone, Copy, Debug, Default)]
struct Cursor {
    base: u64,
    idx: u64,
    cycle: bool,
}

/// DbC-side state that is not a register.
#[derive(Debug, Default)]
pub struct DbcModel {
    dce_stuck: bool,
    dce_ignores_set: bool,
    evt_idx: u64,
    evt_cycle: bool,
    out: Cursor,
    in_ep: Cursor,
    /// The IN TRB the host is part way through filling: (TRB address, buffer, length, bytes so far).
    in_open: Option<(u64, u64, u32, u32)>,
    /// Most event slots in use at once: written by the controller, not yet acknowledged through ERDP.
    evt_high_water: u64,
    /// Events written while the ring had no free slot beyond the one that tells full from empty.
    evt_overflows: u64,
    /// Transfer TRBs longer than 64 KiB or whose buffer crosses a 64 KiB boundary.
    trb_violations: u64,
}

impl State {
    fn get(&self, addr: u64) -> u32 {
        self.mem.get(&addr).copied().unwrap_or(0)
    }

    fn dma_read(&mut self, bus: u64) -> Option<u32> {
        let p = self.dma.translate(bus, 4)?;
        // SAFETY: `translate` checked four bytes at `bus` lie in a live region.
        Some(unsafe { p.cast::<u32>().as_ptr().read_unaligned() })
    }

    fn dma_read64(&mut self, bus: u64) -> Option<u64> {
        Some(u64::from(self.dma_read(bus)?) | (u64::from(self.dma_read(bus + 4)?) << 32))
    }

    fn dma_write(&mut self, bus: u64, value: u32) -> Option<()> {
        let p = self.dma.translate(bus, 4)?;
        // SAFETY: as in `dma_read`.
        unsafe { p.cast::<u32>().as_ptr().write_unaligned(value) };
        Some(())
    }

    /// Write one event TRB into the driver's event ring, cycle dword last.
    fn push_event(&mut self, trb: [u32; 4]) -> Option<()> {
        if self.get(DBC + DCCTRL) & CTRL_DCE == 0 {
            return None;
        }
        let erstba = u64::from(self.get(DBC + DCERSTBA)) | (u64::from(self.get(DBC + DCERSTBA + 4)) << 32);
        let ring = self.dma_read64(erstba)?;
        let size = u64::from(self.dma_read(erstba + 8)?);
        let acked = (u64::from(self.get(DBC + DCERDP)) & !0xf).saturating_sub(ring) / 16;
        let model = self.dbc.as_mut()?;
        let used = (model.evt_idx + size - acked % size) % size;
        model.evt_high_water = model.evt_high_water.max(used + 1);
        if used + 1 >= size {
            model.evt_overflows += 1;
        }
        let slot = ring + model.evt_idx * 16;
        let cycle = u32::from(model.evt_cycle);
        model.evt_idx += 1;
        if model.evt_idx == size {
            model.evt_idx = 0;
            model.evt_cycle = !model.evt_cycle;
        }
        for (i, d) in trb.iter().take(3).enumerate() {
            self.dma_write(slot + 4 * i as u64, *d)?;
        }
        self.dma_write(slot + 12, (trb[3] & !1) | cycle)
    }

    fn port_event(&mut self) {
        self.push_event([DEBUG_PORT << 24, 0, 0, TRB_PORT_STATUS_EVENT << 10]);
    }

    fn transfer_event(&mut self, ptr: u64, cc: u8, residual: u32, ep: u32) -> Option<()> {
        self.push_event([
            ptr as u32,
            (ptr >> 32) as u32,
            (u32::from(cc) << 24) | residual,
            (TRB_TRANSFER_EVENT << 10) | (ep << 16),
        ])
    }

    /// The IN TRB being filled, taking the next posted one if none is open.
    fn open_in_trb(&mut self) -> Option<(u64, u64, u32, u32)> {
        if let Some(open) = self.dbc.as_ref()?.in_open {
            return Some(open);
        }
        let (at, buf, len) = self.next_trb(false)?;
        let open = (at, buf, len, 0);
        self.dbc.as_mut()?.in_open = Some(open);
        Some(open)
    }

    /// Complete the open IN TRB.
    fn finish_in(&mut self, at: u64, cc: u8, residual: u32) -> Option<()> {
        self.dbc.as_mut()?.in_open = None;
        self.transfer_event(at, cc, residual, EP_IN)
    }

    /// Next posted TRB on a transfer ring as (address, buffer, length), following link TRBs.
    fn next_trb(&mut self, out: bool) -> Option<(u64, u64, u32)> {
        for _ in 0..2 {
            let model = self.dbc.as_ref()?;
            let cur = if out { model.out } else { model.in_ep };
            let at = cur.base + cur.idx * 16;
            let d3 = self.dma_read(at + 12)?;
            if (d3 & 1 != 0) != cur.cycle {
                return None;
            }
            let model = self.dbc.as_mut()?;
            let cur = if out { &mut model.out } else { &mut model.in_ep };
            if (d3 >> 10) & 0x3f == TRB_LINK {
                cur.idx = 0;
                if d3 & 2 != 0 {
                    cur.cycle = !cur.cycle;
                }
                continue;
            }
            cur.idx += 1;
            let buf = self.dma_read64(at)?;
            let len = self.dma_read(at + 8)? & 0x1_ffff;
            let crosses = len > 0 && buf / 0x1_0000 != (buf + u64::from(len) - 1) / 0x1_0000;
            if len > 0x1_0000 || crosses {
                self.dbc.as_mut()?.trb_violations += 1;
            }
            return Some((at, buf, len));
        }
        None
    }
}

/// Apply DbC register semantics to a write; false when `addr` is not a DbC register.
pub fn write(st: &mut State, addr: u64, value: u32) -> bool {
    if st.dbc.is_none() || !(DBC..DBC + 0x40).contains(&addr) {
        return false;
    }
    match addr - DBC {
        DCCTRL => write_ctrl(st, value),
        DCST => {},
        DCPORTSC => {
            let old = st.get(DBC + DCPORTSC);
            let mut new = old & !(value & PORT_CHANGES);
            if old & PORT_CCS != 0 {
                new = (new & !PORT_PED) | (value & PORT_PED);
            }
            st.mem.insert(DBC + DCPORTSC, new);
        },
        _ => {
            st.mem.insert(addr, value);
        },
    }
    true
}

/// DCE and LSE are RW, DRC is RW1C, HOT and HIT are RW1S while running, the rest read-only.
fn write_ctrl(st: &mut State, value: u32) {
    let old = st.get(DBC + DCCTRL);
    let Some(model) = st.dbc.as_mut() else {
        return;
    };
    let mut new = old;
    let was_on = old & CTRL_DCE != 0;
    let want_on = value & CTRL_DCE != 0;
    let blocked = (was_on && !want_on && model.dce_stuck) || (!was_on && want_on && model.dce_ignores_set);
    if !blocked {
        new = (new & !CTRL_DCE) | (value & CTRL_DCE);
    }
    new = (new & !CTRL_LSE) | (value & CTRL_LSE);
    if value & CTRL_DRC != 0 {
        new &= !CTRL_DRC;
    }
    if new & CTRL_DCR != 0 {
        new |= value & CTRL_HALTS;
    }
    let now_on = new & CTRL_DCE != 0;
    if was_on && !now_on {
        new &= !(CTRL_DCR | CTRL_DRC | CTRL_HALTS | CTRL_DEV_ADDR);
        st.mem.insert(DBC + DCPORTSC, 0);
        st.mem.insert(DBC + DCST, 0);
    }
    if !was_on && now_on {
        model.evt_idx = 0;
        model.evt_cycle = true;
    }
    st.mem.insert(DBC + DCCTRL, new);
}

impl Sim {
    /// An Intel xHCI at [`DBC_DEV`] with a Debug Capability, legacy support,
    /// USB 3 root ports 1 and 2 and USB 2 ports 3 and 4, all idle.
    #[must_use]
    pub fn with_dbc_controller() -> Self {
        let sim = Self::new();
        sim.add_xhci(DBC_DEV, MMIO);
        sim.poke(MMIO, 0x0100_0020);
        sim.poke(MMIO + 0x04, MAX_PORTS << 24);
        sim.poke(MMIO + 0x10, ((XECP / 4) as u32) << 16);
        sim.poke(LEGACY, 0x0000_0401);
        sim.poke(LEGACY + 0x10, 0x0300_0402);
        sim.poke(LEGACY + 0x18, 0x0000_0201);
        sim.poke(LEGACY + 0x20, 0x0200_0402);
        sim.poke(LEGACY + 0x28, 0x0000_0203);
        sim.poke(DBC, 0x0000_000a);
        sim.poke(DBC + DCCTRL, CTRL_MAX_BURST);
        for port in 0..u64::from(MAX_PORTS) {
            sim.poke(MMIO + 0x20 + 0x400 + port * 0x10, 0x0000_02a0);
        }
        sim.0.borrow_mut().dbc = Some(DbcModel::default());
        sim
    }

    /// Base of the simulated DbC register block.
    #[must_use]
    pub const fn dbc_base(&self) -> u64 {
        DBC
    }

    #[must_use]
    pub fn dcctrl(&self) -> u32 {
        self.peek(DBC + DCCTRL)
    }

    #[must_use]
    pub fn dcportsc(&self) -> u32 {
        self.peek(DBC + DCPORTSC)
    }

    /// Most event ring slots the controller had in use at once since the DbC was created.
    #[must_use]
    pub fn event_high_water(&self) -> u64 {
        self.0.borrow().dbc.as_ref().map_or(0, |m| m.evt_high_water)
    }

    /// Events the controller wrote with the event ring already full.
    #[must_use]
    pub fn event_overflows(&self) -> u64 {
        self.0.borrow().dbc.as_ref().map_or(0, |m| m.evt_overflows)
    }

    /// Transfer TRBs the controller met that a real one may refuse: over 64 KiB, or a buffer across a 64 KiB boundary.
    #[must_use]
    pub fn trb_violations(&self) -> u64 {
        self.0.borrow().dbc.as_ref().map_or(0, |m| m.trb_violations)
    }

    /// DCE will not clear once set.
    pub fn set_dce_stuck(&self, stuck: bool) {
        if let Some(m) = self.0.borrow_mut().dbc.as_mut() {
            m.dce_stuck = stuck;
        }
    }

    /// DCE will not set.
    pub fn set_dce_ignores_set(&self, ignore: bool) {
        if let Some(m) = self.0.borrow_mut().dbc.as_mut() {
            m.dce_ignores_set = ignore;
        }
    }

    /// The debug host attaches at `SuperSpeed`.
    pub fn connect(&self) {
        let mut st = self.0.borrow_mut();
        st.mem.insert(DBC + DCPORTSC, PORT_CCS | PORT_PED | PORT_CSC | PORT_SUPERSPEED);
        st.mem.insert(DBC + DCST, DEBUG_PORT << 24);
        st.port_event();
    }

    /// The debug host configures the device; the DbC loads both endpoint contexts.
    pub fn configure(&self) {
        let mut st = self.0.borrow_mut();
        let ctrl = st.get(DBC + DCCTRL);
        st.mem.insert(DBC + DCCTRL, ctrl | CTRL_DCR | (10 << 24));
        let dccp = u64::from(st.get(DBC + DCCP)) | (u64::from(st.get(DBC + DCCP + 4)) << 32);
        let out = st.dma_read64(dccp + 64 + 8).unwrap_or(0);
        let inn = st.dma_read64(dccp + 128 + 8).unwrap_or(0);
        if let Some(m) = st.dbc.as_mut() {
            m.out = Cursor { base: out & !0xf, idx: 0, cycle: out & 1 != 0 };
            m.in_ep = Cursor { base: inn & !0xf, idx: 0, cycle: inn & 1 != 0 };
            m.in_open = None;
        }
    }

    /// The debug host resets the port: configuration is lost and PRC is set.
    pub fn port_reset(&self) {
        let mut st = self.0.borrow_mut();
        let portsc = st.get(DBC + DCPORTSC);
        st.mem.insert(DBC + DCPORTSC, portsc | PORT_PRC);
        let ctrl = st.get(DBC + DCCTRL);
        st.mem.insert(DBC + DCCTRL, (ctrl & !(CTRL_DCR | CTRL_HALTS)) | CTRL_DRC);
        st.port_event();
    }

    /// The cable is pulled.
    pub fn unplug(&self) {
        let mut st = self.0.borrow_mut();
        st.mem.insert(DBC + DCPORTSC, PORT_CSC);
        st.mem.insert(DBC + DCST, 0);
        let ctrl = st.get(DBC + DCCTRL);
        st.mem.insert(DBC + DCCTRL, (ctrl & !(CTRL_DCR | CTRL_HALTS)) | CTRL_DRC);
        st.port_event();
    }

    /// A host controller reset clears DCE behind the driver's back.
    pub fn reset_controller(&self) {
        let mut st = self.0.borrow_mut();
        st.mem.insert(DBC + DCCTRL, CTRL_MAX_BURST);
        st.mem.insert(DBC + DCPORTSC, 0);
        st.mem.insert(DBC + DCST, 0);
    }

    /// Put an arbitrary transfer event on the event ring.
    pub fn push_transfer_event(&self, ptr: u64, cc: u8, residual: u32) -> bool {
        self.0.borrow_mut().transfer_event(ptr, cc, residual, EP_OUT).is_some()
    }

    /// Complete the next posted OUT TRB and return its payload and TRB address.
    #[must_use]
    pub fn complete_out(&self) -> Option<(u64, Vec<u8>)> {
        self.complete_out_with(CC_SUCCESS)
    }

    /// As [`Sim::complete_out`], reporting completion code `cc`.
    #[must_use]
    pub fn complete_out_with(&self, cc: u8) -> Option<(u64, Vec<u8>)> {
        let mut st = self.0.borrow_mut();
        let (at, buf, len) = st.next_trb(true)?;
        let bytes = (0..u64::from(len))
            .map(|i| {
                let word = st.dma_read(buf + (i & !3))?;
                Some(word.to_le_bytes()[(i & 3) as usize])
            })
            .collect::<Option<Vec<u8>>>()?;
        st.transfer_event(at, cc, 0, EP_OUT)?;
        Some((at, bytes))
    }

    /// The host writes `data` as one transfer, as a bulk OUT: full packets then
    /// a short one if `data` is not a multiple of 1024 bytes, and a zero-length
    /// packet after it when `zlp` is set. Each packet lands in the posted IN
    /// TRBs the way an xHC places it: a TRB completes when it is full (Success)
    /// or when a short packet ends it (Short Packet, with the bytes left over as
    /// residual). A write ending on a full packet without `zlp` leaves the
    /// current TRB open. False when the data or the terminator found no posted TRB.
    #[must_use]
    pub fn host_write(&self, data: &[u8], zlp: bool) -> bool {
        let mut st = self.0.borrow_mut();
        let ends_short = zlp || !data.len().is_multiple_of(1024);
        let mut rest = data;
        while !rest.is_empty() {
            let Some((at, buf, len, filled)) = st.open_in_trb() else {
                return false;
            };
            let n = rest.len().min((len - filled) as usize);
            for (i, chunk) in rest[..n].chunks(4).enumerate() {
                let mut word = [0u8; 4];
                word[..chunk.len()].copy_from_slice(chunk);
                if st.dma_write(buf + u64::from(filled) + 4 * i as u64, u32::from_le_bytes(word)).is_none() {
                    return false;
                }
            }
            rest = &rest[n..];
            let filled = filled + n as u32;
            if filled == len {
                if st.finish_in(at, CC_SUCCESS, 0).is_none() {
                    return false;
                }
            } else if let Some(m) = st.dbc.as_mut() {
                m.in_open = Some((at, buf, len, filled));
            }
        }
        if ends_short {
            // A zero-length packet after an exact fill takes a fresh TRB; after a partial fill it closes that one.
            let Some((at, _, len, filled)) = st.open_in_trb() else {
                return false;
            };
            return st.finish_in(at, CC_SHORT_PACKET, len - filled).is_some();
        }
        true
    }

    /// Fill the next posted IN TRB with `data` and complete it. A write that is
    /// a multiple of 1024 bytes and does not fill the TRB leaves it open; see [`Sim::host_write`].
    #[must_use]
    pub fn deliver_in(&self, data: &[u8]) -> bool {
        self.host_write(data, false)
    }

    /// As [`Sim::deliver_in`], followed by a zero-length packet.
    #[must_use]
    pub fn deliver_in_zlp(&self, data: &[u8]) -> bool {
        self.host_write(data, true)
    }
}
