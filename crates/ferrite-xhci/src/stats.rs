//! Counters, milestone times and register snapshots for reports.

use core::fmt;

use arbitrary_int::u6;
use ferrite_hal::{BusAddr, Instant, Mmio, MmioExt, PhysAddr};

use crate::regs::{Code, DcCtrl, DcSt, Ddi1, Ddi2, ErstSz, PortSc, TrbType, dbc};

/// Number of distinct TRB type codes.
pub const TRB_TYPES: usize = 64;

/// When each milestone was first observed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timings {
    pub dce_ready: Option<Instant>,
    pub connected: Option<Instant>,
    pub configured: Option<Instant>,
}

/// Counters since `open`. Byte counts balance:
/// `bytes_queued == bytes_sent + bytes_dropped + Dbc::buffered()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stats {
    pub events: u32,
    /// Event TRB counts indexed by raw TRB type.
    pub event_types: [u32; TRB_TYPES],
    /// OUT transfer events matched to an in-flight TRB.
    pub out_done: u32,
    /// IN transfer events matched to an in-flight TRB.
    pub in_done: u32,
    /// Of those, the ones that delivered no bytes: a zero-length packet, or an error.
    pub in_empty: u32,
    /// Zero-length OUT TDs posted to end the host's read after a full last packet.
    pub out_terminators: u32,
    /// Transfer events whose TRB pointer matched nothing in flight.
    pub unknown_transfer_events: u32,
    pub transfer_errors: u32,
    pub last_error_cc: u8,
    /// Bytes accepted by `write`.
    pub bytes_queued: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    /// Accepted bytes lost to a failed OUT transfer.
    pub bytes_dropped: u64,
    /// Bytes `write` refused because the buffer was full.
    pub bytes_refused: u64,
    pub port_changes: u32,
    pub halts: u32,
    pub rearms: u32,
    pub kicks: u32,
    pub faults: u32,
    pub recoveries: u32,
}

impl Stats {
    /// Each event type seen, as (raw type, name, count).
    pub fn event_type_counts(&self) -> impl Iterator<Item = (u8, Code<TrbType, u6>, u32)> + '_ {
        self.event_types
            .iter()
            .zip(0u8..)
            .filter(|&(&count, _)| count > 0)
            .map(|(&count, ty)| (ty, TrbType::code(u6::new(ty)), count))
    }
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            events: 0,
            event_types: [0; TRB_TYPES],
            out_done: 0,
            in_done: 0,
            in_empty: 0,
            out_terminators: 0,
            unknown_transfer_events: 0,
            transfer_errors: 0,
            last_error_cc: 0,
            bytes_queued: 0,
            bytes_sent: 0,
            bytes_received: 0,
            bytes_dropped: 0,
            bytes_refused: 0,
            port_changes: 0,
            halts: 0,
            rearms: 0,
            kicks: 0,
            faults: 0,
            recoveries: 0,
        }
    }
}

/// Every DbC register at one instant, read without side effects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegSnapshot {
    pub ctrl: DcCtrl,
    pub status: DcSt,
    pub portsc: PortSc,
    pub erstsz: ErstSz,
    pub erstba: BusAddr,
    pub erdp: BusAddr,
    pub dccp: BusAddr,
    pub ddi1: Ddi1,
    pub ddi2: Ddi2,
}

impl RegSnapshot {
    /// Read the DbC register block at `regs`. Failed reads show as all ones.
    pub fn read<H: Mmio>(hal: &mut H, regs: PhysAddr) -> Self {
        Self {
            ctrl: hal.read(regs, dbc::DCCTRL),
            status: hal.read(regs, dbc::DCST),
            portsc: hal.read(regs, dbc::DCPORTSC),
            erstsz: hal.read(regs, dbc::DCERSTSZ),
            erstba: hal.read_dma_ptr(regs, dbc::DCERSTBA),
            erdp: hal.read_dma_ptr(regs, dbc::DCERDP),
            dccp: hal.read_dma_ptr(regs, dbc::DCCP),
            ddi1: hal.read(regs, dbc::DCDDI1),
            ddi2: hal.read(regs, dbc::DCDDI2),
        }
    }
}

/// Four report lines: DCCTRL, DCST, DCPORTSC, then the pointers and device info.
impl fmt::Display for RegSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let c = self.ctrl;
        let p = self.portsc;
        let b = u8::from;
        writeln!(
            f,
            "DCCTRL={:#010x} DCE={} DCR={} LSE={} HOT={} HIT={} DRC={} maxburst={} devaddr={}",
            c.raw_value(),
            b(c.dce()),
            b(c.dcr()),
            b(c.lse()),
            b(c.hot()),
            b(c.hit()),
            b(c.drc()),
            c.max_burst(),
            c.device_address(),
        )?;
        writeln!(f, "DCST={:#010x} debug_port={}", self.status.raw_value(), self.status.debug_port())?;
        writeln!(
            f,
            "DCPORTSC={:#010x} CCS={} PED={} PR={} link={} speed={} changes={:#x}",
            p.raw_value(),
            b(p.ccs()),
            b(p.ped()),
            b(p.pr()),
            p.link(),
            p.speed_code(),
            p.changes(),
        )?;
        write!(
            f,
            "ERSTSZ={} ERSTBA={:#x} ERDP={:#x} DCCP={:#x} DDI1={:#010x} DDI2={:#010x}",
            self.erstsz.entries(),
            self.erstba,
            self.erdp,
            self.dccp,
            self.ddi1.raw_value(),
            self.ddi2.raw_value()
        )
    }
}
