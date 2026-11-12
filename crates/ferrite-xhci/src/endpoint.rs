//! One bulk endpoint: its transfer ring, halt state and doorbell target.

use crate::regs::DcDb;
use crate::ring::ProducerRing;

/// A change in the controller's halt bit for an endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HaltChange {
    Halted,
    /// The halt cleared; ring this doorbell if transfers are still queued.
    Cleared(Option<DcDb>),
}

pub struct Endpoint {
    pub ring: ProducerRing,
    pub name: &'static str,
    halted: bool,
    doorbell: DcDb,
}

impl Endpoint {
    pub fn new(ring: ProducerRing, name: &'static str, target: u8) -> Self {
        Self { ring, name, halted: false, doorbell: DcDb::ring(target) }
    }

    /// Restart the ring from its initial state and forget any halt.
    pub fn reset(&mut self) {
        self.ring.reset();
        self.halted = false;
    }

    pub const fn halted(&self) -> bool {
        self.halted
    }

    pub const fn doorbell(&self) -> DcDb {
        self.doorbell
    }

    /// Record the controller's halt bit for this endpoint.
    pub fn track_halt(&mut self, halted: bool) -> Option<HaltChange> {
        if self.halted == halted {
            return None;
        }
        self.halted = halted;
        Some(if halted {
            HaltChange::Halted
        } else {
            HaltChange::Cleared((self.ring.pending() > 0).then_some(self.doorbell))
        })
    }
}
