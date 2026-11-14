use core::cell::Cell;

/// The wake-up handshake with a device-side thread that sleeps while its queue is empty.
///
/// The main loop queues frames and must get the thread to run without running any of the thread's code
/// itself: it asks [`Sleeper::wake_needed`] and, only if that says so, marks the thread runnable in the
/// scheduler. Everything else (the sleeping, the queue) stays on the thread. Single-threaded by
/// construction: the thread and the main loop never run at once, so the thread cannot be between its last
/// look at the queue and its sleep when a frame is queued.
#[derive(Default)]
pub struct Sleeper {
    asleep: Cell<bool>,
}

impl Sleeper {
    /// The thread found its queue empty and is about to block.
    pub fn going_to_sleep(&self) {
        self.asleep.set(true);
    }

    /// The thread has work and is running.
    pub fn awake(&self) {
        self.asleep.set(false);
    }

    /// Work was queued: whether the thread is asleep and must be made runnable. Each sleep is woken
    /// once, so a burst of frames marks the thread runnable once, and a thread that is busy, or blocked
    /// on something else, is left alone.
    pub fn wake_needed(&self) -> bool {
        self.asleep.replace(false)
    }
}

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;
    use alloc::rc::Rc;
    use core::cell::Cell;

    use assert2::check;
    use ferrite_hal::Instant;
    use ferrite_net::QueuePort;
    use ferrite_net::port::Port as _;

    use super::*;

    /// A port whose kick wakes `sleeper`'s thread, counting the wakes, and the thread's pull of the queue.
    struct Rig {
        port: Rc<QueuePort>,
        sleeper: Rc<Sleeper>,
        wakes: Rc<Cell<u32>>,
    }

    impl Rig {
        fn new() -> Self {
            let port = Rc::new(QueuePort::new([2, 0, 0, 0, 0, 1], 1500, 8, 8));
            let (sleeper, wakes) = (Rc::new(Sleeper::default()), Rc::new(Cell::new(0)));
            let (kick_sleeper, kick_wakes) = (Rc::clone(&sleeper), Rc::clone(&wakes));
            port.set_kick(Some(Box::new(move || {
                if kick_sleeper.wake_needed() {
                    kick_wakes.set(kick_wakes.get() + 1);
                }
            })));
            Self { port, sleeper, wakes }
        }

        fn queue(&self) {
            self.port.clone().transmit(Instant::default(), &[0; 60]).expect("room in the queue");
        }

        /// What the thread does once: take a frame, or note that it is going to sleep.
        fn thread_step(&self) -> bool {
            if self.port.next_tx().is_some() {
                self.sleeper.awake();
                self.port.transmitted(true);
                true
            } else {
                self.sleeper.going_to_sleep();
                false
            }
        }
    }

    #[test]
    fn a_sleeping_thread_is_woken_once_per_sleep() {
        let rig = Rig::new();
        check!(!rig.thread_step(), "nothing queued: it sleeps");
        rig.queue();
        rig.queue();
        check!(rig.wakes.get() == 1, "two frames, one wake");
        check!(rig.thread_step() && rig.thread_step(), "it drains both");
        check!(!rig.thread_step());
        rig.queue();
        check!(rig.wakes.get() == 2, "the next sleep is woken again");
    }

    #[test]
    fn a_thread_that_is_not_asleep_is_not_woken() {
        let rig = Rig::new();
        rig.queue();
        check!(rig.wakes.get() == 0, "it has not gone to sleep yet, so it will see the frame itself");
        check!(rig.thread_step());
        rig.queue();
        check!(rig.wakes.get() == 0, "running between frames, it is not asleep either");
    }
}
