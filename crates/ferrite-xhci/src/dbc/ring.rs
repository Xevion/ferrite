//! Single-segment rings over DMA memory: producer ring with a link TRB, event
//! ring consumer, and a byte ring feeding the OUT endpoint.

use core::ptr::{self, addr_of, addr_of_mut};
use core::sync::atomic::{Ordering, fence};

use super::trb::{TRB_CYCLE, Trb};

/// TRBs per ring segment (one 4 KiB page).
pub const RING_TRBS: usize = 256;
const TRB_BYTES: u64 = size_of::<Trb>() as u64;

/// Transfer ring with cycle-bit ownership. The last slot is the link TRB, so
/// `RING_TRBS - 1` transfers can be in flight.
pub struct ProducerRing {
    trbs: *mut Trb,
    dma: u64,
    enq: usize,
    deq: usize,
    cycle: bool,
    pending: usize,
    tags: [u16; RING_TRBS],
}

impl ProducerRing {
    /// `trbs` must point at `RING_TRBS` TRBs of DMA memory at bus address `dma`.
    #[must_use]
    pub fn new(trbs: *mut Trb, dma: u64) -> Self {
        let mut ring = Self { trbs, dma, enq: 0, deq: 0, cycle: true, pending: 0, tags: [0; RING_TRBS] };
        ring.reset();
        ring
    }

    /// Zero the ring, rewrite the link TRB and restart at cycle state 1.
    pub fn reset(&mut self) {
        for i in 0..RING_TRBS {
            self.write_trb(i, Trb::default());
        }
        self.write_trb(RING_TRBS - 1, Trb::link(self.dma));
        self.enq = 0;
        self.deq = 0;
        self.cycle = true;
        self.pending = 0;
    }

    #[must_use]
    pub const fn dma(&self) -> u64 {
        self.dma
    }

    #[must_use]
    pub const fn pending(&self) -> usize {
        self.pending
    }

    #[must_use]
    pub const fn free(&self) -> usize {
        RING_TRBS - 1 - self.pending
    }

    #[must_use]
    pub const fn contains(&self, addr: u64) -> bool {
        addr >= self.dma && addr < self.dma + RING_TRBS as u64 * TRB_BYTES
    }

    /// Queue a TRB tagged with `tag` (returned by `complete`). False when full.
    pub fn push(&mut self, mut trb: Trb, tag: u16) -> bool {
        if self.free() == 0 {
            return false;
        }
        // Hand the TRB over with the wrong cycle bit first, then flip it, so the
        // controller never sees a half-written TRB as valid.
        trb.set_cycle(!self.cycle);
        self.write_trb(self.enq, trb);
        fence(Ordering::SeqCst);
        self.set_cycle_bit(self.enq, self.cycle);

        self.tags[self.enq] = tag;
        self.pending += 1;
        self.enq += 1;
        if self.enq == RING_TRBS - 1 {
            self.set_cycle_bit(RING_TRBS - 1, self.cycle);
            self.enq = 0;
            self.cycle = !self.cycle;
        }
        true
    }

    /// Retire the oldest in-flight TRB and return its tag.
    pub fn complete(&mut self) -> Option<u16> {
        if self.pending == 0 {
            return None;
        }
        let tag = self.tags[self.deq];
        self.deq += 1;
        if self.deq == RING_TRBS - 1 {
            self.deq = 0;
        }
        self.pending -= 1;
        Some(tag)
    }

    fn write_trb(&mut self, idx: usize, trb: Trb) {
        // SAFETY: idx < RING_TRBS and `trbs` covers RING_TRBS TRBs by contract.
        unsafe { ptr::write_volatile(self.trbs.add(idx), trb) }
    }

    fn set_cycle_bit(&mut self, idx: usize, cycle: bool) {
        // SAFETY: as in `write_trb`; only the dword holding the cycle bit is touched.
        unsafe {
            let word = addr_of_mut!((*self.trbs.add(idx)).d[3]);
            let v = ptr::read_volatile(word);
            ptr::write_volatile(word, (v & !TRB_CYCLE) | u32::from(cycle));
        }
    }
}

/// Consumer side of the event ring (one segment, no link TRB).
pub struct EventRing {
    trbs: *mut Trb,
    dma: u64,
    deq: usize,
    cycle: bool,
}

impl EventRing {
    /// `trbs` must point at `RING_TRBS` TRBs of DMA memory at bus address `dma`.
    #[must_use]
    pub fn new(trbs: *mut Trb, dma: u64) -> Self {
        let mut ring = Self { trbs, dma, deq: 0, cycle: true };
        ring.reset();
        ring
    }

    pub fn reset(&mut self) {
        for i in 0..RING_TRBS {
            // SAFETY: i < RING_TRBS and `trbs` covers RING_TRBS TRBs by contract.
            unsafe { ptr::write_volatile(self.trbs.add(i), Trb::default()) }
        }
        self.deq = 0;
        self.cycle = true;
    }

    #[must_use]
    pub const fn dma(&self) -> u64 {
        self.dma
    }

    /// Bus address of the next event to consume, for ERDP.
    #[must_use]
    pub const fn dequeue_dma(&self) -> u64 {
        self.dma + self.deq as u64 * TRB_BYTES
    }

    /// Take the next event the controller has produced, if any.
    pub fn pop(&mut self) -> Option<Trb> {
        // SAFETY: deq < RING_TRBS and `trbs` covers RING_TRBS TRBs by contract.
        let slot = unsafe { self.trbs.add(self.deq) };
        let word3 = unsafe { ptr::read_volatile(addr_of!((*slot).d[3])) };
        if (word3 & TRB_CYCLE != 0) != self.cycle {
            return None;
        }
        fence(Ordering::Acquire);
        let trb = unsafe { ptr::read_volatile(slot) };
        self.deq += 1;
        if self.deq == RING_TRBS {
            self.deq = 0;
            self.cycle = !self.cycle;
        }
        Some(trb)
    }
}

/// Power-of-two byte ring. `wr`, `sent` and `acked` are free-running counters:
/// bytes in `[acked, sent)` are on the wire, `[sent, wr)` are waiting for a TRB.
pub struct ByteRing {
    buf: *mut u8,
    dma: u64,
    cap: usize,
    wr: usize,
    sent: usize,
    acked: usize,
}

impl ByteRing {
    /// `buf` must point at `cap` bytes of DMA memory at bus address `dma`; `cap` is a power of two.
    #[must_use]
    pub const fn new(buf: *mut u8, dma: u64, cap: usize) -> Self {
        assert!(cap.is_power_of_two());
        Self { buf, dma, cap, wr: 0, sent: 0, acked: 0 }
    }

    pub const fn reset(&mut self) {
        self.wr = 0;
        self.sent = 0;
        self.acked = 0;
    }

    /// Bytes written but not yet acknowledged by a transfer event.
    #[must_use]
    pub const fn buffered(&self) -> usize {
        self.wr.wrapping_sub(self.acked)
    }

    #[must_use]
    pub const fn free(&self) -> usize {
        self.cap - self.buffered()
    }

    /// Copy as much of `data` as fits; returns the number of bytes taken.
    pub fn write(&mut self, data: &[u8]) -> usize {
        let n = data.len().min(self.free());
        for (i, &b) in data[..n].iter().enumerate() {
            // SAFETY: the index is masked into the `cap` byte buffer.
            unsafe { ptr::write_volatile(self.buf.add(self.wr.wrapping_add(i) & (self.cap - 1)), b) }
        }
        self.wr = self.wr.wrapping_add(n);
        n
    }

    /// Next contiguous run of unsent bytes (at most `max`) as (bus address, length).
    #[must_use]
    pub const fn next_chunk(&self, max: usize) -> Option<(u64, usize)> {
        let unsent = self.wr.wrapping_sub(self.sent);
        if unsent == 0 {
            return None;
        }
        let off = self.sent & (self.cap - 1);
        let mut len = self.cap - off;
        if unsent < len {
            len = unsent;
        }
        if max < len {
            len = max;
        }
        Some((self.dma + off as u64, len))
    }

    pub const fn mark_sent(&mut self, n: usize) {
        self.sent = self.sent.wrapping_add(n);
    }

    pub const fn ack(&mut self, n: usize) {
        self.acked = self.acked.wrapping_add(n);
    }
}

#[cfg(test)]
mod tests {
    use std::vec;
    use std::vec::Vec;

    use super::*;

    fn trbs() -> Vec<Trb> {
        vec![Trb::default(); RING_TRBS]
    }

    #[test]
    fn producer_wraps_and_toggles_cycle() {
        let mut mem = trbs();
        let mut ring = ProducerRing::new(mem.as_mut_ptr(), 0x1000);
        assert_eq!(mem[RING_TRBS - 1].trb_type(), 6);

        for i in 0..RING_TRBS - 1 {
            assert!(ring.push(Trb::normal(0x9000, 8), i as u16));
        }
        assert!(!ring.push(Trb::normal(0x9000, 8), 0), "full");
        assert!(mem[..RING_TRBS - 1].iter().all(Trb::cycle));
        assert!(mem[RING_TRBS - 1].cycle(), "link carries the lap's cycle");

        for i in 0..RING_TRBS - 1 {
            assert_eq!(ring.complete(), Some(i as u16));
        }
        assert_eq!(ring.complete(), None);

        assert!(ring.push(Trb::normal(0xa000, 8), 7));
        assert!(!mem[0].cycle(), "second lap writes cycle 0");
        assert_eq!(mem[0].pointer(), 0xa000);
    }

    #[test]
    fn producer_membership_uses_bus_address() {
        let mut mem = trbs();
        let ring = ProducerRing::new(mem.as_mut_ptr(), 0x1000);
        assert!(ring.contains(0x1000));
        assert!(ring.contains(0x1000 + 255 * 16));
        assert!(!ring.contains(0x2000));
    }

    #[test]
    fn event_ring_consumes_across_wrap() {
        let mut mem = trbs();
        let mut ring = EventRing::new(mem.as_mut_ptr(), 0x4000);
        assert!(ring.pop().is_none());

        for (i, slot) in mem.iter_mut().enumerate() {
            slot.d = [i as u32, 0, 0, (32 << 10) | 1];
        }
        for i in 0..RING_TRBS {
            assert_eq!(ring.pop().map(|t| t.d[0]), Some(i as u32));
        }
        assert!(ring.pop().is_none(), "lap two has not been produced yet");
        assert_eq!(ring.dequeue_dma(), 0x4000);

        mem[0].d[3] = 32 << 10;
        assert_eq!(ring.pop().map(|t| t.d[0]), Some(0));
        assert_eq!(ring.dequeue_dma(), 0x4010);
    }

    #[test]
    fn byte_ring_chunks_split_at_wrap() {
        let mut mem = vec![0u8; 16];
        let mut ring = ByteRing::new(mem.as_mut_ptr(), 0x100, 16);

        assert_eq!(ring.write(&[1; 12]), 12);
        assert_eq!(ring.next_chunk(5), Some((0x100, 5)));
        ring.mark_sent(12);
        ring.ack(12);

        assert_eq!(ring.write(&[2; 10]), 10);
        assert_eq!(ring.next_chunk(1024), Some((0x100 + 12, 4)));
        ring.mark_sent(4);
        assert_eq!(ring.next_chunk(1024), Some((0x100, 6)));
        ring.mark_sent(6);
        assert_eq!(ring.next_chunk(1024), None);
        assert_eq!(&mem[12..], &[2; 4]);
    }

    #[test]
    fn byte_ring_bounds_unacked_bytes() {
        let mut mem = vec![0u8; 8];
        let mut ring = ByteRing::new(mem.as_mut_ptr(), 0, 8);
        assert_eq!(ring.write(&[0; 20]), 8);
        assert_eq!(ring.write(&[0; 1]), 0);
        ring.mark_sent(3);
        assert_eq!(ring.write(&[0; 1]), 0, "sent but unacked bytes still occupy space");
        ring.ack(3);
        assert_eq!(ring.write(&[0; 5]), 3);
    }
}
