//! The smoltcp [`Device`] over the firmware's Simple Network Protocol.
//!
//! SNP may keep reading a transmit buffer after `transmit` returns, until
//! `GetStatus` hands it back as recycled. Frames are therefore sent from a
//! fixed pool owned here, and a slot is reused only once the firmware has
//! returned it.

use alloc::boxed::Box;
use alloc::vec;
use core::time::Duration;

use ferrite_hal::Instant;
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use uefi::Status;
use uefi::boot::ScopedProtocol;
use uefi::proto::network::snp::{NetworkState, SimpleNetwork};

/// Largest Ethernet frame passed to or from the firmware: 14 byte header,
/// 1500 byte MTU, and room for a VLAN tag.
pub const FRAME_CAP: usize = 1536;
pub const TX_SLOTS: usize = 8;
/// A slot the firmware has not returned after this long is reused anyway, so
/// firmware that never reports recycled buffers slows sending instead of stopping it.
const TX_STALE: Duration = Duration::from_secs(1);
/// Bound on recycled buffers drained per poll, against firmware returning the same one forever.
const MAX_RECYCLE_PER_POLL: usize = 4 * TX_SLOTS;

/// Fixed transmit buffers and which of them the firmware still holds.
pub struct TxPool {
    bufs: Box<[[u8; FRAME_CAP]]>,
    /// When each slot was handed to the firmware, if it has not come back yet.
    sent_at: [Option<Instant>; TX_SLOTS],
}

impl TxPool {
    #[must_use]
    pub fn new() -> Self {
        Self { bufs: vec![[0; FRAME_CAP]; TX_SLOTS].into_boxed_slice(), sent_at: [None; TX_SLOTS] }
    }

    pub fn free_slot(&self) -> Option<usize> {
        self.sent_at.iter().position(Option::is_none)
    }

    pub fn in_flight(&self) -> usize {
        self.sent_at.iter().flatten().count()
    }

    /// The first `len` bytes of `slot`, clamped to [`FRAME_CAP`].
    pub fn frame_mut(&mut self, slot: usize, len: usize) -> &mut [u8] {
        &mut self.bufs[slot][..len.min(FRAME_CAP)]
    }

    pub fn mark_sent(&mut self, slot: usize, now: Instant) {
        self.sent_at[slot] = Some(now);
    }

    /// Free the slot whose buffer starts at `ptr`. Returns whether it was one of ours.
    pub fn recycle(&mut self, ptr: *const u8) -> bool {
        let Some(slot) = self.bufs.iter().position(|buf| buf.as_ptr() == ptr) else {
            return false;
        };
        self.sent_at[slot] = None;
        true
    }

    /// Free every slot held for at least `max_age`. Returns how many were freed.
    pub fn reclaim_stale(&mut self, now: Instant, max_age: Duration) -> usize {
        let mut freed = 0;
        for sent in &mut self.sent_at {
            if sent.is_some_and(|at| now - at >= max_age) {
                *sent = None;
                freed += 1;
            }
        }
        freed
    }
}

impl Default for TxPool {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-device counters folded into the network status.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeviceStats {
    pub rx_frames: u32,
    pub rx_errors: u32,
    pub tx_errors: u32,
    /// Slots reused because the firmware never returned them.
    pub tx_stale_reclaims: u32,
}

pub struct SnpDevice {
    snp: ScopedProtocol<SimpleNetwork>,
    /// State the adapter was in when we opened it; restored on drop.
    initial_state: NetworkState,
    rx: Box<[u8; FRAME_CAP]>,
    tx: TxPool,
    now: Instant,
    pub stats: DeviceStats,
}

impl SnpDevice {
    pub fn new(snp: ScopedProtocol<SimpleNetwork>, initial_state: NetworkState) -> Self {
        Self {
            snp,
            initial_state,
            rx: Box::new([0; FRAME_CAP]),
            tx: TxPool::new(),
            now: Instant::default(),
            stats: DeviceStats::default(),
        }
    }

    pub fn snp(&self) -> &SimpleNetwork {
        &self.snp
    }

    /// Take back every transmit buffer the firmware has finished with. The
    /// `GetStatus` call this makes also refreshes `media_present` on EDK2's `SnpDxe`.
    pub fn reclaim(&mut self, now: Instant) {
        self.now = now;
        for _ in 0..MAX_RECYCLE_PER_POLL {
            match self.snp.get_recycled_transmit_buffer_status() {
                Ok(Some(buf)) => {
                    if !self.tx.recycle(buf.as_ptr()) {
                        log::debug!("net: firmware recycled unknown tx buffer {:p}", buf.as_ptr());
                    }
                },
                Ok(None) => break,
                Err(e) => {
                    log::debug!("net: GetStatus failed: {e}");
                    break;
                },
            }
        }
    }

    pub fn link_up(&self) -> bool {
        let mode = self.snp.mode();
        bool::from(mode.media_present) || !bool::from(mode.media_present_supported)
    }

    /// A free transmit slot, reclaiming recycled and then stale ones when the pool is full.
    fn tx_slot(&mut self) -> Option<usize> {
        if let Some(slot) = self.tx.free_slot() {
            return Some(slot);
        }
        self.reclaim(self.now);
        if let Some(slot) = self.tx.free_slot() {
            return Some(slot);
        }
        let freed = self.tx.reclaim_stale(self.now, TX_STALE);
        if freed > 0 {
            if self.stats.tx_stale_reclaims == 0 {
                log::warn!("net: firmware did not recycle {freed} tx buffers within {TX_STALE:?}, reusing them");
            }
            self.stats.tx_stale_reclaims = self.stats.tx_stale_reclaims.saturating_add(freed as u32);
        }
        self.tx.free_slot()
    }
}

impl Drop for SnpDevice {
    fn drop(&mut self) {
        self.reclaim(self.now);
        if self.tx.in_flight() > 0 {
            log::warn!("net: closing SNP with {} tx buffers still held by firmware", self.tx.in_flight());
        }
        let state = self.snp.mode().state;
        if state == NetworkState::INITIALIZED
            && self.initial_state != NetworkState::INITIALIZED
            && let Err(e) = self.snp.shutdown()
        {
            log::warn!("net: SNP shutdown failed: {e}");
        }
        if self.initial_state == NetworkState::STOPPED
            && self.snp.mode().state == NetworkState::STARTED
            && let Err(e) = self.snp.stop()
        {
            log::warn!("net: SNP stop failed: {e}");
        }
        log::debug!("net: SNP state {:?} -> {:?} on close", state, self.snp.mode().state);
    }
}

impl Device for SnpDevice {
    type RxToken<'a>
        = SnpRxToken<'a>
    where
        Self: 'a;
    type TxToken<'a>
        = SnpTxToken<'a>
    where
        Self: 'a;

    fn receive(&mut self, _now: smoltcp::time::Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        // Without a slot for the reply, leave the frame queued in the firmware.
        let slot = self.tx_slot()?;
        match self.snp.receive(&mut self.rx[..], None, None, None, None) {
            Ok(len) => {
                self.stats.rx_frames = self.stats.rx_frames.wrapping_add(1);
                let rx = SnpRxToken { frame: &self.rx[..len.min(FRAME_CAP)] };
                let tx = SnpTxToken {
                    snp: &self.snp,
                    pool: &mut self.tx,
                    slot,
                    now: self.now,
                    tx_errors: &mut self.stats.tx_errors,
                };
                Some((rx, tx))
            },
            Err(e) if e.status() == Status::NOT_READY => None,
            Err(e) => {
                self.stats.rx_errors = self.stats.rx_errors.wrapping_add(1);
                log::debug!("net: SNP receive failed: {e}");
                None
            },
        }
    }

    fn transmit(&mut self, _now: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
        let slot = self.tx_slot()?;
        Some(SnpTxToken {
            snp: &self.snp,
            pool: &mut self.tx,
            slot,
            now: self.now,
            tx_errors: &mut self.stats.tx_errors,
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = (self.snp.mode().max_packet_size as usize).clamp(1514, FRAME_CAP);
        caps
    }
}

pub struct SnpRxToken<'a> {
    frame: &'a [u8],
}

impl RxToken for SnpRxToken<'_> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(self.frame)
    }
}

pub struct SnpTxToken<'a> {
    snp: &'a SimpleNetwork,
    pool: &'a mut TxPool,
    slot: usize,
    now: Instant,
    tx_errors: &'a mut u32,
}

impl TxToken for SnpTxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let frame = self.pool.frame_mut(self.slot, len);
        let result = f(frame);
        // smoltcp wrote the Ethernet header itself, so the firmware adds none.
        match self.snp.transmit(0, frame, None, None, None) {
            Ok(()) => self.pool.mark_sent(self.slot, self.now),
            Err(_) => *self.tx_errors = self.tx_errors.wrapping_add(1),
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    const fn ms(ms: u64) -> Instant {
        Instant::from_epoch(Duration::from_millis(ms))
    }

    #[test]
    fn slots_return_only_through_recycle_or_staleness() {
        let mut pool = TxPool::new();
        for slot in 0..TX_SLOTS {
            check!(pool.free_slot() == Some(slot));
            pool.mark_sent(slot, ms(100));
        }
        check!(pool.free_slot().is_none());
        check!(pool.in_flight() == TX_SLOTS);

        let third = pool.frame_mut(3, 64).as_ptr();
        check!(pool.recycle(third));
        check!(pool.free_slot() == Some(3));
        check!(!pool.recycle(third.wrapping_add(1)));

        pool.mark_sent(3, ms(900));
        check!(pool.reclaim_stale(ms(1_099), TX_STALE) == 0);
        check!(pool.reclaim_stale(ms(1_100), TX_STALE) == TX_SLOTS - 1);
        check!(pool.in_flight() == 1);
        check!(pool.reclaim_stale(ms(1_900), TX_STALE) == 1);
    }

    #[test]
    fn frames_are_clamped_to_the_slot() {
        let mut pool = TxPool::new();
        check!(pool.frame_mut(0, 60).len() == 60);
        check!(pool.frame_mut(0, FRAME_CAP + 100).len() == FRAME_CAP);
    }
}
