//! A network port: anything that sends and receives Ethernet frames.
//!
//! The firmware's Simple Network Protocol (wired NICs) and a Wi-Fi station's netdev queues are both
//! ports, and the IP stack runs over any of them the same way, one stack per port, several at once.

use core::fmt;

use ferrite_hal::Instant;

/// The Ethernet payload size every port carries at most.
pub const MTU: usize = 1500;
pub const ETH_HEADER: usize = 14;
/// The largest frame a port carries: header and MTU, no FCS.
pub const FRAME_MAX: usize = ETH_HEADER + MTU;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct MacAddr([u8; 6]);

impl MacAddr {
    #[must_use]
    pub const fn new(octets: [u8; 6]) -> Self {
        Self(octets)
    }

    #[must_use]
    pub const fn octets(self) -> [u8; 6] {
        self.0
    }
}

impl fmt::Display for MacAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [first, rest @ ..] = self.0;
        write!(f, "{first:02x}")?;
        rest.iter().try_for_each(|octet| write!(f, ":{octet:02x}"))
    }
}

/// What carries a port's frames, for display.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Medium {
    /// A wired NIC driven by the firmware.
    Ethernet,
    /// A Wi-Fi station associated with an access point.
    WiFi,
}

/// Why a frame was not sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxError {
    /// No room right now; the frame is dropped and counted.
    Busy,
    /// Larger than the port carries.
    TooLarge,
    /// The device refused it.
    Failed,
}

/// Frame counters since the port was opened.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PortStats {
    pub rx_frames: u64,
    /// Received frames lost before the stack saw them: queue full, too large, device error.
    pub rx_dropped: u64,
    pub tx_frames: u64,
    /// Frames the stack sent that never reached the device: no room, too large.
    pub tx_dropped: u64,
    /// Frames the device refused or failed to send.
    pub tx_errors: u64,
}

impl fmt::Display for PortStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "rx {} (dropped {}), tx {} (dropped {}, errors {})",
            self.rx_frames, self.rx_dropped, self.tx_frames, self.tx_dropped, self.tx_errors
        )
    }
}

/// A source and sink of Ethernet frames. Object safe, so a set of ports of different kinds can be
/// held together; every method is non-blocking.
pub trait Port {
    fn medium(&self) -> Medium;
    fn mac(&self) -> MacAddr;
    /// Payload size, at most [`MTU`].
    fn mtu(&self) -> usize;
    /// Whether the port can carry frames now: cable in, or associated with the port open.
    fn link_up(&self) -> bool;
    /// Housekeeping at `now`, before frames move: reclaim transmit buffers, read link state.
    fn service(&mut self, now: Instant);
    /// Copy the next received frame into `buf` (at least [`FRAME_MAX`] bytes) and return its length.
    fn receive(&mut self, buf: &mut [u8]) -> Option<usize>;
    /// Whether [`Port::transmit`] would take a frame now.
    fn can_transmit(&self) -> bool;
    /// Send `frame`, a whole Ethernet frame without FCS.
    fn transmit(&mut self, now: Instant, frame: &[u8]) -> Result<(), TxError>;
    fn stats(&self) -> PortStats;
}
