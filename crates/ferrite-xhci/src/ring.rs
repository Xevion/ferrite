//! Single-segment rings over DMA memory: producer ring with a link TRB, event
//! ring consumer, and a byte ring feeding the OUT endpoint.
//!
//! TRBs are read and written one dword at a time, cycle dword last on the
//! producer side and first on the consumer side, so neither the CPU nor the
//! controller ever acts on a half-written TRB.

use core::sync::atomic::{Ordering, fence};

use ferrite_hal::{BusAddr, BusAddr32};

use crate::dma::DmaSlice;
use crate::trb::Trb;

/// TRBs per ring segment (one 4 KiB page).
pub const RING_TRBS: usize = 256;
pub const RING_BYTES: usize = RING_TRBS * TRB_BYTES;
const TRB_BYTES: usize = size_of::<Trb>();
/// Producer slots usable for transfers; the last one holds the link TRB.
const USABLE: usize = RING_TRBS - 1;
/// Byte offset of the dword holding the cycle bit.
const CYCLE_DWORD: usize = 12;

/// Check `mem` is exactly one 16-byte aligned ring segment.
fn check_segment(mem: DmaSlice) -> DmaSlice {
    assert!(mem.len() == RING_BYTES, "ring memory must be one {RING_BYTES} byte segment");
    assert!(mem.bus().get().is_multiple_of(TRB_BYTES as u32), "ring bus address must be 16-byte aligned");
    assert!(mem.ptr().cast::<Trb>().is_aligned(), "ring memory must be 16-byte aligned");
    mem
}

/// Store dwords 0 to 3 of `trb` in slot `idx`, in that order.
fn write_trb(mem: DmaSlice, idx: usize, trb: Trb) {
    for (k, d) in trb.d.iter().enumerate() {
        mem.write(idx * TRB_BYTES + 4 * k, *d);
    }
}

/// Transfer ring with cycle-bit ownership. The last slot is the link TRB, so
/// `RING_TRBS - 1` transfers can be in flight.
pub struct ProducerRing {
    mem: DmaSlice,
    enq: usize,
    deq: usize,
    cycle: bool,
    pending: usize,
    tags: [u16; RING_TRBS],
}

impl ProducerRing {
    pub fn new(mem: DmaSlice) -> Self {
        let mut ring = Self { mem: check_segment(mem), enq: 0, deq: 0, cycle: true, pending: 0, tags: [0; RING_TRBS] };
        ring.reset();
        ring
    }

    /// Zero the ring, rewrite the link TRB and restart at cycle state 1.
    pub fn reset(&mut self) {
        self.mem.zero();
        write_trb(self.mem, USABLE, Trb::link(self.mem.bus()));
        self.enq = 0;
        self.deq = 0;
        self.cycle = true;
        self.pending = 0;
    }

    #[must_use]
    pub const fn dma(&self) -> BusAddr32 {
        self.mem.bus()
    }

    #[must_use]
    pub const fn pending(&self) -> usize {
        self.pending
    }

    #[must_use]
    pub const fn free(&self) -> usize {
        USABLE - self.pending
    }

    /// Queue a TRB tagged with `tag` (handed back on completion). False when full.
    pub fn push(&mut self, mut trb: Trb, tag: u16) -> bool {
        if self.free() == 0 {
            return false;
        }
        // Hand the TRB over with the wrong cycle bit first, then flip it.
        trb.set_cycle(!self.cycle);
        write_trb(self.mem, self.enq, trb);
        fence(Ordering::SeqCst);
        self.set_cycle_bit(self.enq, self.cycle);

        self.tags[self.enq] = tag;
        self.pending += 1;
        self.enq += 1;
        if self.enq == USABLE {
            self.set_cycle_bit(USABLE, self.cycle);
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
        self.deq = (self.deq + 1) % USABLE;
        self.pending -= 1;
        Some(tag)
    }

    /// Retire every in-flight TRB up to and including the one at bus address
    /// `ptr`, passing each tag to `retire` oldest first. Returns how many were
    /// retired, or `None` (retiring nothing) when `ptr` is not an in-flight TRB.
    pub fn complete_to(&mut self, ptr: BusAddr, mut retire: impl FnMut(u16)) -> Option<usize> {
        let idx = self.in_flight_distance(ptr)?;
        for _ in 0..=idx {
            retire(self.complete()?);
        }
        Some(idx + 1)
    }

    /// Position of `ptr` counted from the oldest in-flight TRB.
    fn in_flight_distance(&self, ptr: BusAddr) -> Option<usize> {
        let off = ptr.get().checked_sub(u64::from(self.dma().get()))?;
        if !off.is_multiple_of(TRB_BYTES as u64) {
            return None;
        }
        let idx = usize::try_from(off / TRB_BYTES as u64).ok().filter(|&i| i < USABLE)?;
        let dist = (idx + USABLE - self.deq) % USABLE;
        (dist < self.pending).then_some(dist)
    }

    fn set_cycle_bit(&self, idx: usize, cycle: bool) {
        let off = idx * TRB_BYTES + CYCLE_DWORD;
        let v: u32 = self.mem.read(off);
        self.mem.write(off, (v & !1) | u32::from(cycle));
    }
}

/// Consumer side of the event ring (one segment, no link TRB).
pub struct EventRing {
    mem: DmaSlice,
    deq: usize,
    cycle: bool,
}

impl EventRing {
    pub fn new(mem: DmaSlice) -> Self {
        let mut ring = Self { mem: check_segment(mem), deq: 0, cycle: true };
        ring.reset();
        ring
    }

    pub fn reset(&mut self) {
        self.mem.zero();
        self.deq = 0;
        self.cycle = true;
    }

    #[must_use]
    pub const fn dma(&self) -> BusAddr32 {
        self.mem.bus()
    }

    /// Bus address of the next event to consume, for ERDP.
    #[must_use]
    pub fn dequeue_dma(&self) -> BusAddr32 {
        self.mem.bus_at(self.deq * TRB_BYTES)
    }

    /// Take the next event the controller has produced, if any.
    pub fn pop(&mut self) -> Option<Trb> {
        let base = self.deq * TRB_BYTES;
        let d3: u32 = self.mem.read(base + CYCLE_DWORD);
        if (d3 & 1 != 0) != self.cycle {
            return None;
        }
        fence(Ordering::Acquire);
        let trb = Trb { d: [self.mem.read(base), self.mem.read(base + 4), self.mem.read(base + 8), d3] };
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
    mem: DmaSlice,
    cap: usize,
    wr: usize,
    sent: usize,
    acked: usize,
}

impl ByteRing {
    pub fn new(mem: DmaSlice) -> Self {
        assert!(mem.len().is_power_of_two(), "byte ring capacity must be a power of two");
        Self { mem, cap: mem.len(), wr: 0, sent: 0, acked: 0 }
    }

    /// Start every counter at `start`, to exercise counter wraparound.
    #[cfg(test)]
    pub fn with_counters(mem: DmaSlice, start: usize) -> Self {
        Self { wr: start, sent: start, acked: start, ..Self::new(mem) }
    }

    /// Requeue every unacknowledged byte, for after the OUT ring was reset.
    pub const fn rewind(&mut self) {
        self.sent = self.acked;
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
        let off = self.wr & (self.cap - 1);
        let (head, tail) = data[..n].split_at(n.min(self.cap - off));
        self.mem.copy_from(off, head);
        self.mem.copy_from(0, tail);
        self.wr = self.wr.wrapping_add(n);
        n
    }

    /// Bus address of the first byte of the ring.
    #[must_use]
    pub const fn bus(&self) -> BusAddr32 {
        self.mem.bus()
    }

    /// Bytes written but not yet handed to a TRB.
    #[must_use]
    pub const fn unsent(&self) -> usize {
        self.wr.wrapping_sub(self.sent)
    }

    /// Next contiguous run of unsent bytes (at most `max`) as (bus address, length).
    #[must_use]
    pub fn next_chunk(&self, max: usize) -> Option<(BusAddr32, usize)> {
        let unsent = self.unsent();
        if unsent == 0 {
            return None;
        }
        let off = self.sent & (self.cap - 1);
        let len = (self.cap - off).min(unsent).min(max);
        Some((self.mem.bus_at(off), len))
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

    use core::ptr::NonNull;

    use arbitrary_int::u17;
    use assert2::{assert, check};

    use super::*;

    const BASE: u64 = 0x1000;

    fn slice<T>(mem: &mut [T], bus: u32) -> DmaSlice {
        // SAFETY: every test keeps `mem` alive for as long as the ring built on it.
        unsafe { DmaSlice::from_raw(NonNull::from(&mut mem[0]).cast(), BusAddr32::from_u32(bus), size_of_val(mem)) }
    }

    fn bus(addr: u32) -> BusAddr32 {
        BusAddr32::from_u32(addr)
    }

    fn trbs() -> Vec<Trb> {
        vec![Trb::default(); RING_TRBS]
    }

    fn normal(addr: u32) -> Trb {
        Trb::normal(bus(addr), u17::new(8))
    }

    fn at(i: usize) -> BusAddr {
        BusAddr::new(BASE + (i * TRB_BYTES) as u64)
    }

    #[test]
    fn producer_wraps_and_toggles_cycle() {
        let mut mem = trbs();
        let mut ring = ProducerRing::new(slice(&mut mem, BASE as u32));
        check!(mem[USABLE].control().trb_type_raw().value() == 6);

        for i in 0..USABLE {
            assert!(ring.push(normal(0x9000), i as u16));
        }
        assert!(!ring.push(normal(0x9000), 0), "full");
        check!(mem[..USABLE].iter().all(Trb::cycle));
        check!(mem[USABLE].cycle(), "link carries the lap's cycle");

        for i in 0..USABLE {
            check!(ring.complete() == Some(i as u16));
        }
        check!(ring.complete() == None);

        assert!(ring.push(normal(0xa000), 7));
        check!(!mem[0].cycle(), "second lap writes cycle 0");
        check!(mem[0].pointer() == BusAddr::new(0xa000));
    }

    #[test]
    fn producer_laps_two_and_three() {
        let mut mem = trbs();
        let mut ring = ProducerRing::new(slice(&mut mem, BASE as u32));
        let mut tag = 0u16;
        for lap in 1..=3 {
            let lap_cycle = lap % 2 == 1;
            for _ in 0..USABLE {
                assert!(ring.push(normal(0x9000), tag));
                assert!(ring.complete() == Some(tag), "complete wraps with the producer");
                tag = tag.wrapping_add(1);
            }
            check!(mem[..USABLE].iter().all(|t| t.cycle() == lap_cycle), "lap {lap} cycle");
            check!(mem[USABLE].cycle() == lap_cycle, "lap {lap} link cycle");
            check!(mem[USABLE].control().toggle_cycle(), "link keeps toggle-cycle on lap {lap}");
            check!(mem[USABLE].pointer() == BusAddr::new(BASE));
        }
        check!(ring.pending() == 0);
    }

    #[test]
    fn interleaved_push_and_complete() {
        let mut mem = trbs();
        let mut ring = ProducerRing::new(slice(&mut mem, BASE as u32));
        let mut next_push = 0u16;
        let mut next_done = 0u16;
        for round in 0..300 {
            for _ in 0..3 {
                assert!(ring.push(normal(0x9000), next_push));
                next_push += 1;
            }
            for _ in 0..(if round % 2 == 0 { 2 } else { 4 }) {
                if let Some(tag) = ring.complete() {
                    assert!(tag == next_done, "completions come back in order");
                    next_done += 1;
                }
            }
            check!(ring.pending() == usize::from(next_push - next_done));
        }
    }

    #[test]
    fn complete_to_retires_through_the_pointer() {
        let mut mem = trbs();
        let mut ring = ProducerRing::new(slice(&mut mem, BASE as u32));
        for tag in 0..5 {
            assert!(ring.push(normal(0x9000), tag));
        }
        let mut got = Vec::new();
        check!(ring.complete_to(at(2), |t| got.push(t)) == Some(3));
        check!(got == [0, 1, 2]);
        check!(ring.pending() == 2);
    }

    #[test]
    fn complete_to_rejects_stale_and_foreign_pointers() {
        let mut mem = trbs();
        let mut ring = ProducerRing::new(slice(&mut mem, BASE as u32));
        for tag in 0..4 {
            assert!(ring.push(normal(0x9000), tag));
        }
        check!(ring.complete_to(at(1), |_| {}) == Some(2));

        let mut touched = false;
        let raw = |addr: u64| BusAddr::new(addr);
        for ptr in [at(0), at(1), at(4), at(USABLE), at(RING_TRBS), raw(BASE - 16), raw(at(2).get() + 8), raw(0)] {
            check!(ring.complete_to(ptr, |_| touched = true) == None, "ptr {ptr:#x}");
        }
        check!(!touched);
        check!(ring.pending() == 2, "rejected events retire nothing");
    }

    #[test]
    fn complete_to_follows_the_wrap() {
        let mut mem = trbs();
        let mut ring = ProducerRing::new(slice(&mut mem, BASE as u32));
        for tag in 0..250 {
            assert!(ring.push(normal(0x9000), tag));
        }
        check!(ring.complete_to(at(249), |_| {}) == Some(250));
        for tag in 250..260 {
            assert!(ring.push(normal(0x9000), tag));
        }
        let mut got = Vec::new();
        check!(ring.complete_to(at(2), |t| got.push(t)) == Some(8), "slots 250..254 then 0..2");
        check!(got == [250, 251, 252, 253, 254, 255, 256, 257]);
        check!(ring.complete_to(at(253), |_| {}) == None, "already retired on this lap");
    }

    #[test]
    fn event_ring_consumes_across_wrap() {
        let mut mem = trbs();
        let mut ring = EventRing::new(slice(&mut mem, 0x4000));
        check!(ring.pop().is_none());

        for (i, slot) in mem.iter_mut().enumerate() {
            slot.d = [i as u32, 0, 0, (32 << 10) | 1];
        }
        for i in 0..RING_TRBS {
            check!(ring.pop().map(|t| t.d[0]) == Some(i as u32));
        }
        check!(ring.pop().is_none(), "lap two has not been produced yet");
        check!(ring.dequeue_dma() == bus(0x4000));

        mem[0].d[3] = 32 << 10;
        check!(ring.pop().map(|t| t.d[0]) == Some(0));
        check!(ring.dequeue_dma() == bus(0x4010));
    }

    #[test]
    fn byte_ring_chunks_split_at_wrap() {
        let mut mem = vec![0u8; 16];
        let mut ring = ByteRing::new(slice(&mut mem, 0x100));

        check!(ring.write(&[1; 12]) == 12);
        check!(ring.next_chunk(5) == Some((bus(0x100), 5)));
        ring.mark_sent(12);
        ring.ack(12);

        check!(ring.write(&[2; 10]) == 10);
        check!(ring.next_chunk(1024) == Some((bus(0x100 + 12), 4)));
        ring.mark_sent(4);
        check!(ring.next_chunk(1024) == Some((bus(0x100), 6)));
        ring.mark_sent(6);
        check!(ring.next_chunk(1024) == None);
        check!(mem[12..] == [2; 4]);
    }

    #[test]
    fn byte_ring_bounds_unacked_bytes() {
        let mut mem = vec![0u8; 8];
        let mut ring = ByteRing::new(slice(&mut mem, 0));
        check!(ring.write(&[0; 20]) == 8);
        check!(ring.write(&[0; 1]) == 0);
        ring.mark_sent(3);
        check!(ring.write(&[0; 1]) == 0, "sent but unacked bytes still occupy space");
        ring.ack(3);
        check!(ring.write(&[0; 5]) == 3);
    }

    #[test]
    fn byte_ring_counters_wrap_past_usize_max() {
        let mut mem = vec![0u8; 16];
        let mut ring = ByteRing::with_counters(slice(&mut mem, 0x100), usize::MAX - 5);
        check!(ring.write(&[3; 12]) == 12);
        check!(ring.buffered() == 12);
        check!(ring.next_chunk(1024) == Some((bus(0x100 + 10), 6)), "offset is the counter masked to the buffer");
        ring.mark_sent(6);
        check!(ring.next_chunk(1024) == Some((bus(0x100), 6)));
        ring.mark_sent(6);
        ring.ack(4);
        check!(ring.buffered() == 8);
        check!(ring.free() == 8);
    }

    #[test]
    fn rewind_requeues_unacked_bytes() {
        let mut mem = vec![0u8; 16];
        let mut ring = ByteRing::new(slice(&mut mem, 0x100));
        check!(ring.write(&[1; 10]) == 10);
        ring.mark_sent(10);
        ring.ack(4);
        ring.rewind();
        check!(ring.next_chunk(1024) == Some((bus(0x104), 6)), "the six unacked bytes go out again");
        check!(ring.buffered() == 6);
    }
}
