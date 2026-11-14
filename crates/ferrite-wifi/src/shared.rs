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

/// State shared between the station service and the main loop: what the station is doing, the port it
/// offers, and the request to stop. Single-threaded by construction: the two sides never run at once.
pub struct Shared {
    state: RefCell<StationState>,
    slot: RefCell<Slot>,
    stop: Cell<bool>,
}

impl Shared {
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            state: RefCell::new(StationState::Starting),
            slot: RefCell::new(Slot { current: None, generation: 0, seen: 0, announced: false }),
            stop: Cell::new(false),
        })
    }

    pub fn state(&self) -> StationState {
        self.state.borrow().clone()
    }

    /// [`StationState::label`] of the current state, without cloning it.
    pub fn label(&self) -> &'static str {
        self.state.borrow().label()
    }

    pub fn set_state(&self, state: StationState) {
        *self.state.borrow_mut() = state;
    }

    /// Offer `port`, replacing any older one the main loop has not read yet.
    pub fn publish(&self, port: Rc<QueuePort>) {
        let mut slot = self.slot.borrow_mut();
        slot.current = Some(port);
        slot.generation += 1;
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

    pub fn request_stop(&self) {
        self.stop.set(true);
    }

    pub fn stop_requested(&self) -> bool {
        self.stop.get()
    }
}
