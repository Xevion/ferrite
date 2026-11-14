//! A [`Port`] that hands every frame sent back to itself, without allocating after construction.
//!
//! A stack on it talks to its own sockets, so a server and a client can share one interface in a
//! test or a self-check.

use alloc::boxed::Box;

use ferrite_hal::Instant;

use crate::port::{ETH_HEADER, FRAME_MAX, MTU, MacAddr, Medium, Port, PortStats, TxError};

const SLOTS: usize = 64;

#[derive(Clone, Copy)]
struct Slot {
    len: usize,
    data: [u8; FRAME_MAX],
}

pub struct LoopbackPort {
    ring: Box<[Slot]>,
    head: usize,
    count: usize,
    mac: MacAddr,
    tx_room: bool,
    stats: PortStats,
}

impl LoopbackPort {
    #[must_use]
    pub fn new() -> Self {
        Self::with_mac(MacAddr::new([0x02, 0, 0, 0, 0, 1]))
    }

    #[must_use]
    pub fn with_mac(mac: MacAddr) -> Self {
        let empty = Slot { len: 0, data: [0; FRAME_MAX] };
        Self {
            ring: alloc::vec![empty; SLOTS].into_boxed_slice(),
            head: 0,
            count: 0,
            mac,
            tx_room: true,
            stats: PortStats::default(),
        }
    }

    /// Queue `frame` as if it had arrived from the wire.
    pub fn push_rx(&mut self, frame: &[u8]) {
        if self.count == SLOTS || frame.len() > FRAME_MAX {
            self.stats.rx_dropped += 1;
            return;
        }
        let at = (self.head + self.count) % SLOTS;
        self.ring[at].len = frame.len();
        self.ring[at].data[..frame.len()].copy_from_slice(frame);
        self.count += 1;
    }

    /// Make [`Port::can_transmit`] answer `room`, to test a port with no transmit buffer free.
    pub fn set_tx_room(&mut self, room: bool) {
        self.tx_room = room;
    }
}

impl Default for LoopbackPort {
    fn default() -> Self {
        Self::new()
    }
}

impl Port for LoopbackPort {
    fn medium(&self) -> Medium {
        Medium::Ethernet
    }

    fn mac(&self) -> MacAddr {
        self.mac
    }

    fn mtu(&self) -> usize {
        MTU
    }

    fn link_up(&self) -> bool {
        true
    }

    fn service(&mut self, _now: Instant) {}

    fn receive(&mut self, buf: &mut [u8]) -> Option<usize> {
        if self.count == 0 {
            return None;
        }
        let slot = &self.ring[self.head];
        let len = slot.len.min(buf.len());
        buf[..len].copy_from_slice(&slot.data[..len]);
        self.head = (self.head + 1) % SLOTS;
        self.count -= 1;
        self.stats.rx_frames += 1;
        Some(len)
    }

    fn can_transmit(&mut self) -> bool {
        self.tx_room && self.count < SLOTS
    }

    fn transmit(&mut self, _now: Instant, frame: &[u8]) -> Result<(), TxError> {
        if frame.len() > ETH_HEADER + MTU {
            self.stats.tx_dropped += 1;
            return Err(TxError::TooLarge);
        }
        if !self.can_transmit() {
            self.stats.tx_dropped += 1;
            return Err(TxError::Busy);
        }
        self.push_rx(frame);
        self.stats.tx_frames += 1;
        Ok(())
    }

    fn stats(&self) -> PortStats {
        self.stats
    }
}
