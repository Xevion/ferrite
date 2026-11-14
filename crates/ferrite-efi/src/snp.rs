//! The firmware's Simple Network Protocol as a [`ferrite_net::Port`].
//!
//! SNP may keep reading a transmit buffer after `transmit` returns, until `GetStatus` hands it back
//! as recycled. Frames are therefore sent from a fixed pool owned here, and a slot is reused only
//! once the firmware has returned it. Opening a handle exclusively stops the firmware's own drivers
//! above it, so a port reconnects them when it is dropped.
//!
//! `wait_for_packet_event` never fires under QEMU, so receiving polls `receive` (an empty queue is
//! `NOT_READY`) instead of waiting on the event.

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::time::Duration;

use ferrite_hal::Instant;
use ferrite_net::mdns::MDNS_GROUP;
use ferrite_net::port::{MacAddr, Medium, Port, PortStats, TxError};
use snafu::{ResultExt as _, Snafu};
use uefi::boot::{self, ScopedProtocol};
use uefi::proto::network::EfiMacAddr;
use uefi::proto::network::snp::{NetworkState, ReceiveFlags, SimpleNetwork};
use uefi::{Handle, Status};

/// Largest Ethernet frame passed to the firmware: 14 byte header, 1500 byte MTU, and room for a
/// VLAN tag.
const FRAME_CAP: usize = 1536;
const TX_SLOTS: usize = 8;
/// A slot the firmware has not returned after this long is reused anyway, so firmware that never
/// reports recycled buffers slows sending instead of stopping it.
const TX_STALE: Duration = Duration::from_secs(1);
/// Bound on recycled buffers drained per call, against firmware returning the same one forever.
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

    /// The first `len` bytes of `slot`, clamped to the slot.
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

/// The setup step that failed, and on which SNP handle.
#[derive(Debug, Snafu)]
pub enum SnpError {
    #[snafu(display("locating SNP handles: {source}"))]
    Locate { source: uefi::Error },
    #[snafu(display("opening SNP {handle:?} exclusively: {source}"))]
    Open { handle: Handle, source: uefi::Error },
    #[snafu(display("starting SNP {handle:?}: {source}"))]
    Start { handle: Handle, source: uefi::Error },
    #[snafu(display("initializing SNP {handle:?}: {source}"))]
    Initialize { handle: Handle, source: uefi::Error },
    #[snafu(display("setting receive filters on SNP {handle:?} (supported {supported:?}): {source}"))]
    Filter { handle: Handle, supported: ReceiveFlags, source: uefi::Error },
}

impl SnpError {
    /// A short reason for the screen.
    pub fn reason(&self) -> &'static str {
        let (Self::Locate { source }
        | Self::Open { source, .. }
        | Self::Start { source, .. }
        | Self::Initialize { source, .. }
        | Self::Filter { source, .. }) = self;
        match (self, source.status()) {
            (_, Status::NOT_FOUND) => "no wired NIC driven by the firmware",
            (_, Status::ACCESS_DENIED) => "wired NIC busy (held by the firmware's network stack)",
            (_, Status::UNSUPPORTED) => "wired NIC unsupported by the firmware",
            (Self::Filter { .. }, _) => "wired NIC refused receive filters",
            _ => "wired NIC setup failed",
        }
    }
}

/// Every SNP handle the firmware has, opened as a port, and the handles that could not be.
#[derive(Default)]
pub struct Discovery {
    pub ports: Vec<SnpPort>,
    pub failures: Vec<SnpError>,
}

/// Open every SNP handle as a port. Handles are not skipped for lacking a cable: a port reports its
/// link, and a cable plugged in later brings it up.
pub fn discover() -> Result<Discovery, SnpError> {
    let handles = match boot::find_handles::<SimpleNetwork>() {
        Err(e) if e.status() == Status::NOT_FOUND => return Ok(Discovery::default()),
        found => found.context(LocateSnafu)?,
    };
    log::debug!("net: {} SNP handles", handles.len());
    let mut found = Discovery::default();
    for handle in handles {
        match SnpPort::open(handle) {
            Ok(port) => found.ports.push(port),
            Err(failure) => found.failures.push(failure),
        }
    }
    Ok(found)
}

/// Reconnects the firmware's drivers to the SNP handle on drop.
struct Reconnect(Handle);

impl Drop for Reconnect {
    fn drop(&mut self) {
        match boot::connect_controller(self.0, &[], None, true) {
            Ok(()) => log::debug!("net: firmware drivers reconnected to SNP"),
            Err(e) => log::warn!("net: reconnecting firmware drivers to SNP failed: {e}"),
        }
    }
}

pub struct SnpPort {
    snp: ScopedProtocol<SimpleNetwork>,
    /// State the adapter was in when it was opened; restored on drop.
    initial_state: NetworkState,
    mac: MacAddr,
    tx: TxPool,
    now: Instant,
    stats: PortStats,
    /// Declared last so it runs after `snp` has closed the protocol.
    _reconnect: Reconnect,
}

impl SnpPort {
    /// Open `handle` exclusively, bring the adapter up and set its receive filters.
    pub fn open(handle: Handle) -> Result<Self, SnpError> {
        let snp = boot::open_protocol_exclusive::<SimpleNetwork>(handle).context(OpenSnafu { handle })?;
        let reconnect = Reconnect(handle);
        let initial_state = snp.mode().state;
        let mut octets = [0; 6];
        octets.copy_from_slice(&snp.mode().current_address.0[..6]);
        let port = Self {
            snp,
            initial_state,
            mac: MacAddr::new(octets),
            tx: TxPool::new(),
            now: Instant::default(),
            stats: PortStats::default(),
            _reconnect: reconnect,
        };

        let snp = &*port.snp;
        if snp.mode().state == NetworkState::STOPPED {
            snp.start().context(StartSnafu { handle })?;
        }
        if snp.mode().state != NetworkState::INITIALIZED {
            snp.initialize(0, 0).context(InitializeSnafu { handle })?;
        }
        configure_filters(handle, snp)?;
        log::debug!("net: SNP opened in state {initial_state:?}, mac {}, link {}", port.mac, port.link_up());
        Ok(port)
    }

    /// Take back every transmit buffer the firmware has finished with. The `GetStatus` call this
    /// makes also refreshes `media_present` on EDK2's `SnpDxe`.
    fn reclaim(&mut self) {
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

    /// A free transmit slot, reclaiming recycled and then stale ones when the pool is full.
    fn tx_slot(&mut self) -> Option<usize> {
        if let Some(slot) = self.tx.free_slot() {
            return Some(slot);
        }
        self.reclaim();
        if let Some(slot) = self.tx.free_slot() {
            return Some(slot);
        }
        let freed = self.tx.reclaim_stale(self.now, TX_STALE);
        if freed > 0 {
            if self.stats.tx_reclaimed == 0 {
                log::warn!("net: firmware did not recycle {freed} tx buffers within {TX_STALE:?}, reusing them");
            }
            self.stats.tx_reclaimed = self.stats.tx_reclaimed.saturating_add(freed as u64);
        }
        self.tx.free_slot()
    }
}

impl Drop for SnpPort {
    fn drop(&mut self) {
        self.reclaim();
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
        // A NIC left running may still read a held buffer, so the pool outlives the port.
        if self.tx.in_flight() > 0 && self.snp.mode().state == NetworkState::INITIALIZED {
            core::mem::forget(core::mem::take(&mut self.tx.bufs));
        }
    }
}

impl Port for SnpPort {
    fn medium(&self) -> Medium {
        Medium::Ethernet
    }

    fn mac(&self) -> MacAddr {
        self.mac
    }

    fn mtu(&self) -> usize {
        ferrite_net::port::MTU
    }

    fn link_up(&self) -> bool {
        let mode = self.snp.mode();
        bool::from(mode.media_present) || !bool::from(mode.media_present_supported)
    }

    fn service(&mut self, now: Instant) {
        self.now = now;
        self.reclaim();
    }

    fn receive(&mut self, buf: &mut [u8]) -> Option<usize> {
        let cap = buf.len().min(FRAME_CAP);
        match self.snp.receive(&mut buf[..cap], None, None, None, None) {
            Ok(len) => {
                self.stats.rx_frames += 1;
                Some(len.min(cap))
            },
            Err(e) if e.status() == Status::NOT_READY => None,
            Err(e) => {
                self.stats.rx_dropped += 1;
                log::debug!("net: SNP receive failed: {e}");
                None
            },
        }
    }

    fn can_transmit(&mut self) -> bool {
        self.tx_slot().is_some()
    }

    fn transmit(&mut self, now: Instant, frame: &[u8]) -> Result<(), TxError> {
        self.now = now;
        if frame.len() > FRAME_CAP {
            self.stats.tx_dropped += 1;
            return Err(TxError::TooLarge);
        }
        let Some(slot) = self.tx_slot() else {
            self.stats.tx_dropped += 1;
            return Err(TxError::Busy);
        };
        let buf = self.tx.frame_mut(slot, frame.len());
        buf.copy_from_slice(frame);
        // The Ethernet header is already in the frame, so the firmware adds none.
        match self.snp.transmit(0, buf, None, None, None) {
            Ok(()) => {
                self.tx.mark_sent(slot, now);
                self.stats.tx_frames += 1;
                Ok(())
            },
            Err(e) => {
                self.stats.tx_errors += 1;
                log::debug!("net: SNP transmit failed: {e}");
                Err(TxError::Failed)
            },
        }
    }

    fn stats(&self) -> PortStats {
        self.stats
    }
}

/// `mac` in the firmware's 32-byte form.
fn efi_mac(mac: MacAddr) -> EfiMacAddr {
    let mut efi = EfiMacAddr([0; 32]);
    efi.0[..6].copy_from_slice(&mac.octets());
    efi
}

/// Enable unicast and broadcast, plus the best multicast mode the NIC supports: the mDNS group in
/// the hardware filter, else all multicast, else promiscuous. MULTICAST is tried even when the mode
/// reports no filter slots, because iPXE's SNP never fills in that count and accepts any filter list.
fn configure_filters(handle: Handle, snp: &SimpleNetwork) -> Result<(), SnpError> {
    let mode = snp.mode();
    let supported = ReceiveFlags::from_bits_truncate(mode.receive_filter_mask);
    let base = (ReceiveFlags::UNICAST | ReceiveFlags::BROADCAST) & supported;
    let group = [efi_mac(MacAddr::ipv4_multicast(MDNS_GROUP.octets()))];
    log::debug!(
        "net: receive filters supported {supported:?}, {} multicast slots, currently {:?}",
        mode.max_mcast_filter_count,
        ReceiveFlags::from_bits_truncate(mode.receive_filter_setting)
    );

    let candidates = [
        (ReceiveFlags::MULTICAST, Some(&group[..])),
        (ReceiveFlags::PROMISCUOUS_MULTICAST, None),
        (ReceiveFlags::PROMISCUOUS, None),
        (ReceiveFlags::empty(), None),
    ];
    let mut last_err = None;
    for (multicast, filters) in candidates {
        if !supported.contains(multicast) {
            log::debug!("net: receive filter {multicast:?} not supported, skipped");
            continue;
        }
        let enable = base | multicast;
        match snp.receive_filters(enable, supported - enable, false, filters) {
            Ok(()) if multicast.is_empty() => {
                log::warn!(
                    "net: no multicast receive mode accepted, mDNS works only if the NIC passes multicast anyway"
                );
                return Ok(());
            },
            Ok(()) => {
                log::debug!("net: receive filters set to {enable:?}");
                return Ok(());
            },
            Err(e) => {
                log::debug!("net: receive filters {enable:?} refused with {:?}", e.status());
                last_err = Some(e);
            },
        }
    }
    let source = last_err.unwrap_or_else(|| uefi::Error::from(Status::UNSUPPORTED));
    Err(SnpError::Filter { handle, supported, source })
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
