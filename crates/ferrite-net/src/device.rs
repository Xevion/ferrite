//! The smoltcp [`Device`] over any [`Port`], without allocating.
//!
//! Frames pass through two scratch buffers the stack owns: one for the frame being received and one
//! for the frame being built, so a port never sees smoltcp's tokens and smoltcp never sees a port's
//! buffers.

use alloc::boxed::Box;

use ferrite_hal::Instant;
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};

use crate::port::{ETH_HEADER, FRAME_MAX, Port, TxError};

/// smoltcp's time from the HAL's.
pub fn smol_instant(now: Instant) -> smoltcp::time::Instant {
    smoltcp::time::Instant::from_micros(i64::try_from(now.since_epoch().as_micros()).unwrap_or(i64::MAX))
}

/// The frame being received and the frame being built.
pub struct Scratch {
    rx: [u8; FRAME_MAX],
    tx: [u8; FRAME_MAX],
}

impl Scratch {
    #[must_use]
    pub fn new() -> Box<Self> {
        Box::new(Self { rx: [0; FRAME_MAX], tx: [0; FRAME_MAX] })
    }
}

pub struct PortDevice<'a> {
    port: &'a mut dyn Port,
    scratch: &'a mut Scratch,
    now: Instant,
}

impl<'a> PortDevice<'a> {
    pub fn new(port: &'a mut dyn Port, scratch: &'a mut Scratch, now: Instant) -> Self {
        Self { port, scratch, now }
    }
}

impl Device for PortDevice<'_> {
    type RxToken<'a>
        = FrameRx<'a>
    where
        Self: 'a;
    type TxToken<'a>
        = FrameTx<'a>
    where
        Self: 'a;

    fn receive(&mut self, _now: smoltcp::time::Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        // Without room for a reply, the frame stays queued in the port.
        if !self.port.can_transmit() {
            return None;
        }
        let len = self.port.receive(&mut self.scratch.rx)?.min(FRAME_MAX);
        let Scratch { rx, tx } = &mut *self.scratch;
        Some((FrameRx(&rx[..len]), FrameTx { port: &mut *self.port, buf: tx, now: self.now }))
    }

    fn transmit(&mut self, _now: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
        if !self.port.can_transmit() {
            return None;
        }
        Some(FrameTx { port: &mut *self.port, buf: &mut self.scratch.tx, now: self.now })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = ETH_HEADER + self.port.mtu();
        caps
    }
}

pub struct FrameRx<'a>(&'a [u8]);

impl RxToken for FrameRx<'_> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(self.0)
    }
}

pub struct FrameTx<'a> {
    port: &'a mut dyn Port,
    buf: &'a mut [u8; FRAME_MAX],
    now: Instant,
}

impl TxToken for FrameTx<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let frame = &mut self.buf[..len.min(FRAME_MAX)];
        let result = f(frame);
        // The port counts what it drops or fails to send.
        if let Err(error) = self.port.transmit(self.now, frame)
            && error != TxError::Busy
        {
            log::debug!("net: {} frame of {len} bytes not sent: {error:?}", self.port.medium());
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;
    use crate::loopback::LoopbackPort;

    const ZERO: smoltcp::time::Instant = smoltcp::time::Instant::ZERO;

    #[test]
    fn frames_built_by_smoltcp_come_back_through_receive() {
        let mut port = LoopbackPort::new();
        let mut scratch = Scratch::new();
        let mut device = PortDevice::new(&mut port, &mut scratch, Instant::default());

        let tx = device.transmit(ZERO).expect("room to send");
        tx.consume(60, |frame| frame.fill(0x5a));
        let (rx, _reply) = device.receive(ZERO).expect("the frame sent");
        check!(rx.consume(<[u8]>::to_vec) == [0x5a; 60]);
        check!(device.receive(ZERO).is_none());
    }

    #[test]
    fn a_frame_waits_in_the_port_while_there_is_no_room_to_answer() {
        let mut port = LoopbackPort::new();
        port.push_rx(&[1; 60]);
        port.set_tx_room(false);
        let mut scratch = Scratch::new();
        {
            let mut device = PortDevice::new(&mut port, &mut scratch, Instant::default());
            check!(device.transmit(ZERO).is_none());
            check!(device.receive(ZERO).is_none());
        }
        port.set_tx_room(true);
        let mut device = PortDevice::new(&mut port, &mut scratch, Instant::default());
        let (rx, _) = device.receive(ZERO).expect("the queued frame");
        check!(rx.consume(<[u8]>::len) == 60);
    }

    #[test]
    fn the_mtu_comes_from_the_port() {
        let mut port = LoopbackPort::new();
        let mut scratch = Scratch::new();
        let device = PortDevice::new(&mut port, &mut scratch, Instant::default());
        check!(device.capabilities().max_transmission_unit == ETH_HEADER + 1500);
    }
}
