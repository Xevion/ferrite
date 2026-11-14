//! Networking shared by every app, platform-free: the [`port::Port`] every NIC is seen through, and
//! the IP stack and services that run over ports.
//!
//! A [`QueuePort`] is a port fed by a device that pushes frames (the Wi-Fi start-up does it with a Linux
//! `rx_handler` and a transmit thread): bounded receive and transmit queues with drop counters. It is
//! single-threaded; the device side and the program side must never run at the same time.

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod dhcp;
pub mod http;
pub mod ping;
pub mod port;

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::rc::Rc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use smoltcp::iface::{Config, Interface};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::wire::{EthernetAddress, HardwareAddress};

use crate::port::{ETH_HEADER, MTU, MacAddr, Port, PortStats, TxError};

struct FrameQueue {
    frames: VecDeque<Vec<u8>>,
    cap: usize,
}

impl FrameQueue {
    fn new(cap: usize) -> Self {
        Self { frames: VecDeque::with_capacity(cap), cap }
    }

    /// False, and the frame dropped, when the queue is full.
    fn push(&mut self, frame: Vec<u8>) -> bool {
        if self.frames.len() >= self.cap {
            return false;
        }
        self.frames.push_back(frame);
        true
    }
}

struct State {
    rx: FrameQueue,
    tx: FrameQueue,
    stats: PortStats,
}

/// One network device's frames in both directions.
pub struct QueuePort {
    mac: [u8; 6],
    mtu: usize,
    state: RefCell<State>,
    /// Called after smoltcp queues a frame, so the device side wakes and sends it.
    kick: RefCell<Option<Box<dyn Fn()>>>,
}

impl QueuePort {
    /// A link for a device with address `mac` and payload size `mtu` (at most [`MTU`]), queueing at most
    /// `rx_cap` received and `tx_cap` outgoing frames.
    pub fn new(mac: [u8; 6], mtu: usize, rx_cap: usize, tx_cap: usize) -> Self {
        Self {
            mac,
            mtu: mtu.min(MTU),
            state: RefCell::new(State {
                rx: FrameQueue::new(rx_cap),
                tx: FrameQueue::new(tx_cap),
                stats: PortStats::default(),
            }),
            kick: RefCell::new(None),
        }
    }

    pub const fn mac(&self) -> [u8; 6] {
        self.mac
    }

    pub const fn mtu(&self) -> usize {
        self.mtu
    }

    pub fn set_kick(&self, kick: Option<Box<dyn Fn()>>) {
        if let Ok(mut slot) = self.kick.try_borrow_mut() {
            *slot = kick;
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> Option<R> {
        self.state.try_borrow_mut().ok().map(|mut state| f(&mut state))
    }

    /// A frame the device received, queued for smoltcp unless it is too large or the queue is full.
    pub fn deliver(&self, frame: &[u8]) {
        let fits = frame.len() <= ETH_HEADER + self.mtu;
        if self
            .with(|state| {
                if fits && state.rx.push(frame.to_vec()) {
                    state.stats.rx_frames += 1;
                } else {
                    state.stats.rx_dropped += 1;
                }
            })
            .is_none()
        {
            log::warn!("netif: a received frame arrived while the link was in use; dropped");
        }
    }

    /// A received frame the device could not hand over.
    pub fn count_rx_drop(&self) {
        self.with(|state| state.stats.rx_dropped += 1);
    }

    /// The oldest frame smoltcp queued for sending.
    pub fn next_tx(&self) -> Option<Vec<u8>> {
        self.with(|state| state.tx.frames.pop_front()).flatten()
    }

    /// Whether frames wait for the device side.
    pub fn tx_pending(&self) -> bool {
        self.with(|state| !state.tx.frames.is_empty()).unwrap_or(false)
    }

    /// The device side's verdict on a frame from [`QueuePort::next_tx`].
    pub fn transmitted(&self, ok: bool) {
        self.with(|state| {
            if ok {
                state.stats.tx_frames += 1;
            } else {
                state.stats.tx_errors += 1;
            }
        });
    }

    /// A frame from [`QueuePort::next_tx`] the device side could not take at all.
    pub fn count_tx_drop(&self) {
        self.with(|state| state.stats.tx_dropped += 1);
    }

    pub fn stats(&self) -> PortStats {
        self.with(|state| state.stats).unwrap_or_default()
    }

    fn queue_tx(&self, frame: Vec<u8>) {
        let queued = self.with(|state| {
            let queued = state.tx.push(frame);
            if !queued {
                state.stats.tx_dropped += 1;
            }
            queued
        });
        if queued == Some(true)
            && let Ok(kick) = self.kick.try_borrow()
            && let Some(kick) = kick.as_ref()
        {
            kick();
        }
    }

    fn next_rx(&self) -> Option<Vec<u8>> {
        self.with(|state| state.rx.frames.pop_front()).flatten()
    }

    /// The smoltcp device over this link.
    pub const fn device(&self) -> QueueDevice<'_> {
        QueueDevice(self)
    }

    /// A smoltcp interface with this link's address, at `now`.
    pub fn interface(&self, now: smoltcp::time::Instant, random_seed: u64) -> Interface {
        let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(self.mac)));
        config.random_seed = random_seed;
        Interface::new(config, &mut self.device(), now)
    }
}

impl Port for Rc<QueuePort> {
    fn medium(&self) -> port::Medium {
        port::Medium::WiFi
    }

    fn mac(&self) -> MacAddr {
        MacAddr::new(self.mac)
    }

    fn mtu(&self) -> usize {
        self.mtu
    }

    fn link_up(&self) -> bool {
        true
    }

    fn service(&mut self, _now: ferrite_hal::Instant) {}

    fn receive(&mut self, buf: &mut [u8]) -> Option<usize> {
        let frame = self.next_rx()?;
        let n = frame.len().min(buf.len());
        buf[..n].copy_from_slice(&frame[..n]);
        Some(n)
    }

    fn can_transmit(&self) -> bool {
        self.with(|state| state.tx.frames.len() < state.tx.cap).unwrap_or(false)
    }

    fn transmit(&mut self, _now: ferrite_hal::Instant, frame: &[u8]) -> Result<(), TxError> {
        if frame.len() > ETH_HEADER + self.mtu {
            self.count_tx_drop();
            return Err(TxError::TooLarge);
        }
        if !self.can_transmit() {
            self.count_tx_drop();
            return Err(TxError::Busy);
        }
        self.queue_tx(frame.to_vec());
        Ok(())
    }

    fn stats(&self) -> PortStats {
        QueuePort::stats(self)
    }
}

/// smoltcp's time from the HAL's.
pub fn smol_instant(now: ferrite_hal::Instant) -> smoltcp::time::Instant {
    smoltcp::time::Instant::from_micros(i64::try_from(now.since_epoch().as_micros()).unwrap_or(i64::MAX))
}

pub struct QueueDevice<'a>(&'a QueuePort);

impl Device for QueueDevice<'_> {
    type RxToken<'b>
        = FrameRx
    where
        Self: 'b;
    type TxToken<'b>
        = FrameTx<'b>
    where
        Self: 'b;

    fn receive(&mut self, _now: smoltcp::time::Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let frame = self.0.next_rx()?;
        Some((FrameRx(frame), FrameTx(self.0)))
    }

    fn transmit(&mut self, _now: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
        let full = self.0.with(|state| state.tx.frames.len() >= state.tx.cap).unwrap_or(true);
        (!full).then_some(FrameTx(self.0))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = ETH_HEADER + self.0.mtu;
        caps
    }
}

pub struct FrameRx(Vec<u8>);

impl RxToken for FrameRx {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.0)
    }
}

pub struct FrameTx<'a>(&'a QueuePort);

impl TxToken for FrameTx<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut frame = vec![0; len];
        let result = f(&mut frame);
        self.0.queue_tx(frame);
        result
    }
}

#[cfg(test)]
mod tests {
    use alloc::rc::Rc;
    use core::cell::Cell;

    use assert2::check;

    use super::*;

    #[test]
    fn full_queues_and_oversize_frames_are_counted_not_kept() {
        let link = QueuePort::new([2, 0, 0, 0, 0, 1], 1500, 2, 1);
        let kicks = Rc::new(Cell::new(0));
        let counted = Rc::clone(&kicks);
        link.set_kick(Some(Box::new(move || counted.set(counted.get() + 1))));

        for _ in 0..3 {
            link.deliver(&[0; 60]);
        }
        link.deliver(&[0; port::FRAME_MAX + 1]);
        let mut device = link.device();
        let now = smoltcp::time::Instant::ZERO;
        let (rx, tx) = device.receive(now).expect("a queued frame");
        check!(rx.consume(<[u8]>::len) == 60);
        tx.consume(42, |buf| buf.fill(7));
        check!(device.transmit(now).is_none(), "the one-frame transmit queue is full");
        let (_, tx) = device.receive(now).expect("the second frame");
        tx.consume(42, |_| ());
        check!(device.receive(now).is_none());

        check!(link.next_tx() == Some(vec![7; 42]));
        link.transmitted(true);
        check!(link.next_tx().is_none());
        check!(kicks.get() == 1, "only a queued frame wakes the device side");
        check!(link.stats() == PortStats { rx_frames: 2, rx_dropped: 2, tx_frames: 1, tx_dropped: 1, tx_errors: 0 });
    }
}
