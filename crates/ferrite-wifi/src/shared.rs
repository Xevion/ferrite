use alloc::rc::Rc;
use core::cell::{Cell, RefCell};

use ferrite_net::QueuePort;

use crate::StationState;

/// A change in the port the station offers the main loop.
#[derive(Clone)]
pub enum PortEvent {
    /// A link is open and carries frames. It replaces any port the main loop already holds: the
    /// station dropped and reconnected in between.
    Up(Rc<QueuePort>),
    /// The port the main loop holds is gone; its stack over it should be dropped.
    Down,
}

struct Slot {
    current: Option<Rc<QueuePort>>,
    /// Bumped on every publish and every withdrawal of a held port.
    generation: u64,
    /// The generation the main loop last read.
    seen: u64,
    /// Whether the main loop has been handed a port it has not been told is gone.
    announced: bool,
}

/// State shared between the station service and the main loop.
///
/// It holds what the station is doing, the port it offers, the request to stop, and the verdict of a
/// program that checks the link. Single-threaded by construction: the two sides never run at once.
pub struct Shared {
    state: RefCell<StationState>,
    slot: RefCell<Slot>,
    stop: Cell<bool>,
    verdict: Cell<Option<bool>>,
}

impl Shared {
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            state: RefCell::new(StationState::Starting),
            slot: RefCell::new(Slot { current: None, generation: 0, seen: 0, announced: false }),
            stop: Cell::new(false),
            verdict: Cell::new(None),
        })
    }

    pub fn state(&self) -> StationState {
        self.state.borrow().clone()
    }

    /// [`StationState::label`] of the current state, without cloning it.
    pub fn label(&self) -> &'static str {
        self.state.borrow().label()
    }

    /// Read the current state in place, for a caller that formats it on every render and cannot clone it.
    pub fn with_state<R>(&self, read: impl FnOnce(&StationState) -> R) -> R {
        read(&self.state.borrow())
    }

    pub fn set_state(&self, state: StationState) {
        *self.state.borrow_mut() = state;
    }

    /// Offer `port`, replacing any older one the main loop has not read yet. Any verdict on an earlier
    /// port is forgotten.
    pub fn publish(&self, port: Rc<QueuePort>) {
        let mut slot = self.slot.borrow_mut();
        slot.current = Some(port);
        slot.generation += 1;
        self.verdict.set(None);
    }

    /// Take the offered port back, before the device behind it is detached. The port's kick is cleared
    /// too, so a main loop that still holds it cannot wake a device side that is going away.
    pub fn withdraw(&self) {
        let mut slot = self.slot.borrow_mut();
        if let Some(port) = slot.current.take() {
            port.set_kick(None);
            slot.generation += 1;
        }
    }

    /// What changed since the last call, in the main loop. Several changes between calls collapse to
    /// the latest: a withdrawal followed by a new port reads as `Up`, and a port the main loop never saw
    /// is never reported gone.
    pub fn next_port_event(&self) -> Option<PortEvent> {
        let mut slot = self.slot.borrow_mut();
        if slot.generation == slot.seen {
            return None;
        }
        slot.seen = slot.generation;
        if let Some(port) = slot.current.clone() {
            slot.announced = true;
            return Some(PortEvent::Up(port));
        }
        core::mem::take(&mut slot.announced).then_some(PortEvent::Down)
    }

    /// Whether the main loop has been handed the offered port and not yet told it is gone.
    pub fn port_taken(&self) -> bool {
        self.slot.borrow().announced
    }

    /// The main loop is done with the port: `passed` is whether its checks over it passed. For a program
    /// that tests the link rather than serving over it, which holds the connection until this is called.
    pub fn report(&self, passed: bool) {
        self.verdict.set(Some(passed));
    }

    /// What the main loop reported over the current port, once it has.
    pub fn verdict(&self) -> Option<bool> {
        self.verdict.get()
    }

    pub fn request_stop(&self) {
        self.stop.set(true);
    }

    pub fn stop_requested(&self) -> bool {
        self.stop.get()
    }
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};

    use super::*;

    fn port() -> Rc<QueuePort> {
        Rc::new(QueuePort::new([2, 0, 0, 0, 0, 1], 1500, 4, 4))
    }

    #[test]
    fn a_program_that_checks_the_link_gets_its_verdict_back_per_port() {
        let shared = Shared::new();
        shared.publish(port());
        check!(!shared.port_taken(), "offered, not yet read");
        assert!(let Some(PortEvent::Up(_)) = shared.next_port_event());
        check!(shared.port_taken());
        check!(shared.verdict().is_none());

        shared.report(false);
        check!(shared.verdict() == Some(false));
        shared.withdraw();
        check!(shared.verdict() == Some(false), "the verdict outlives the port that earned it");
        assert!(let Some(PortEvent::Down) = shared.next_port_event());
        check!(!shared.port_taken());

        shared.publish(port());
        check!(shared.verdict().is_none(), "a new port starts without a verdict");
    }
}
