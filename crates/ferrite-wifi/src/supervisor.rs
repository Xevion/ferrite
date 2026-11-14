use alloc::rc::Rc;
use alloc::string::String;
use core::time::Duration;

use ferrite_hal::Instant;
use ferrite_net::QueuePort;
use ferrite_net::port::MacAddr;

use crate::{Failure, Policy, Reconnect, Shared, StationState};

/// What the service does after an attempt ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// Wait this long, then scan and connect again.
    Retry(Duration),
    /// Leave the loop: the service was stopped, or the failure is permanent.
    Done,
}

/// The reconnect state machine. The service reports each step of an attempt; the supervisor keeps the
/// shared state and the offered port in step with it, and decides the wait after a failure.
pub struct Supervisor {
    shared: Rc<Shared>,
    backoff: Reconnect,
    up_since: Option<Instant>,
}

impl Supervisor {
    pub const fn new(shared: Rc<Shared>, policy: Policy) -> Self {
        Self { shared, backoff: Reconnect::new(policy), up_since: None }
    }

    pub fn stop_requested(&self) -> bool {
        self.shared.stop_requested()
    }

    pub fn scanning(&self) {
        self.shared.set_state(StationState::Scanning);
    }

    pub fn connecting(&self, ssid: &str) {
        self.shared.set_state(StationState::Connecting { ssid: String::from(ssid) });
    }

    /// The port opened: offer `port` to the main loop.
    pub fn connected(&mut self, ssid: &str, bssid: MacAddr, port: Rc<QueuePort>, now: Instant) {
        self.up_since = Some(now);
        self.shared.publish(port);
        self.shared.set_state(StationState::Connected { ssid: String::from(ssid), bssid });
    }

    /// The attempt ended in `reason`, before or after the port opened. The port is withdrawn first, so
    /// the main loop drops its stack before the device is detached.
    pub fn failed(&mut self, reason: Failure, now: Instant) -> Next {
        self.shared.withdraw();
        if let Some(since) = self.up_since.take() {
            self.backoff.connection_ended(now - since);
        }
        if reason.is_permanent() {
            self.shared.set_state(StationState::Failed { reason, retry_in: None });
            return Next::Done;
        }
        let wait = self.backoff.next_delay();
        self.shared.set_state(StationState::Failed { reason, retry_in: Some(wait) });
        Next::Retry(wait)
    }

    /// The service was asked to stop and is leaving the network.
    pub fn stopped(&mut self) -> Next {
        self.shared.withdraw();
        self.up_since = None;
        self.shared.set_state(StationState::Stopped);
        Next::Done
    }

    /// The service cannot run at all: `state` is one of the final ones (no card, no credentials, no
    /// firmware).
    pub fn unavailable(&self, state: StationState) {
        self.shared.withdraw();
        self.shared.set_state(state);
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString as _;

    use assert2::{assert, check};

    use super::*;
    use crate::PortEvent;

    fn port(last: u8) -> Rc<QueuePort> {
        Rc::new(QueuePort::new([2, 0, 0, 0, 0, last], 1500, 4, 4))
    }

    fn at(secs: u64) -> Instant {
        Instant::from_epoch(Duration::from_secs(secs))
    }

    const AP: MacAddr = MacAddr::new([0x10, 0, 0, 0, 0, 1]);

    fn station() -> (Rc<Shared>, Supervisor) {
        let shared = Shared::new();
        (Rc::clone(&shared), Supervisor::new(shared, Policy::default()))
    }

    #[test]
    fn a_drop_withdraws_the_port_before_the_retry_and_the_next_port_replaces_it() {
        let (shared, mut station) = station();
        station.scanning();
        station.connecting("home");
        let first = port(1);
        station.connected("home", AP, Rc::clone(&first), at(5));
        assert!(let Some(PortEvent::Up(held)) = shared.next_port_event());
        check!(Rc::ptr_eq(&held, &first));
        check!(shared.state().is_connected());
        check!(shared.next_port_event().is_none(), "an event is reported once");

        check!(station.failed(Failure::Lost, at(65)) == Next::Retry(Duration::from_secs(1)));
        check!(
            shared.state() == StationState::Failed { reason: Failure::Lost, retry_in: Some(Duration::from_secs(1)) }
        );
        assert!(let Some(PortEvent::Down) = shared.next_port_event());

        let second = port(2);
        station.connected("home", AP, Rc::clone(&second), at(70));
        assert!(let Some(PortEvent::Up(held)) = shared.next_port_event());
        check!(Rc::ptr_eq(&held, &second));
    }

    #[test]
    fn a_main_loop_that_polls_late_sees_only_the_latest_port() {
        let (shared, mut station) = station();
        station.connected("home", AP, port(1), at(0));
        assert!(let Some(PortEvent::Up(_)) = shared.next_port_event());
        station.failed(Failure::Lost, at(20));
        let second = port(2);
        station.connected("home", AP, Rc::clone(&second), at(22));
        assert!(let Some(PortEvent::Up(held)) = shared.next_port_event());
        check!(Rc::ptr_eq(&held, &second));
        check!(shared.next_port_event().is_none());
    }

    #[test]
    fn a_port_the_main_loop_never_saw_is_not_reported_gone() {
        let (shared, mut station) = station();
        station.connected("home", AP, port(1), at(0));
        station.failed(Failure::Lost, at(1));
        check!(shared.next_port_event().is_none());
    }

    #[test]
    fn failures_back_off_until_a_stable_connection() {
        let (shared, mut station) = station();
        let waits: [Next; 3] = core::array::from_fn(|_| station.failed(Failure::NotFound, at(0)));
        check!(waits == [1, 2, 4].map(|s| Next::Retry(Duration::from_secs(s))));
        check!(
            shared.state()
                == StationState::Failed { reason: Failure::NotFound, retry_in: Some(Duration::from_secs(4)) }
        );

        station.connected("home", AP, port(1), at(100));
        check!(station.failed(Failure::Lost, at(105)) == Next::Retry(Duration::from_secs(8)), "5 s up is not stable");
        station.connected("home", AP, port(1), at(120));
        check!(station.failed(Failure::Lost, at(140)) == Next::Retry(Duration::from_secs(1)), "20 s up is");
    }

    #[test]
    fn a_withdrawn_port_no_longer_wakes_the_device_side() {
        use alloc::boxed::Box;
        use core::cell::Cell;

        use ferrite_net::port::Port as _;

        let (_shared, mut station) = station();
        let kicks = Rc::new(Cell::new(0));
        let first = port(1);
        let counted = Rc::clone(&kicks);
        first.set_kick(Some(Box::new(move || counted.set(counted.get() + 1))));
        station.connected("home", AP, Rc::clone(&first), at(0));

        let mut held = Rc::clone(&first);
        held.transmit(at(1), &[0; 60]).expect("room in the queue");
        check!(kicks.get() == 1);
        station.failed(Failure::Lost, at(2));
        held.transmit(at(3), &[0; 60]).expect("the queue still takes frames");
        check!(kicks.get() == 1, "the old port's device side is gone");
    }

    #[test]
    fn a_dead_driver_ends_the_service_and_a_stop_leaves_cleanly() {
        let (shared, mut station) = station();
        check!(station.failed(Failure::Driver, at(0)) == Next::Done);
        check!(shared.state() == StationState::Failed { reason: Failure::Driver, retry_in: None });
        check!(shared.state().is_final());

        let (shared, mut station) = station_connected();
        assert!(let Some(PortEvent::Up(_)) = shared.next_port_event());
        shared.request_stop();
        check!(station.stop_requested());
        check!(station.stopped() == Next::Done);
        check!(shared.state() == StationState::Stopped);
        assert!(let Some(PortEvent::Down) = shared.next_port_event());
    }

    fn station_connected() -> (Rc<Shared>, Supervisor) {
        let (shared, mut station) = station();
        station.connected("home", AP, port(1), at(0));
        (shared, station)
    }

    #[test]
    fn states_read_plainly() {
        check!(StationState::NoCard.to_string() == "no Wi-Fi card");
        check!(StationState::Connecting { ssid: "home".into() }.to_string() == "connecting to \"home\"");
        check!(
            StationState::Connected { ssid: "home".into(), bssid: AP }.to_string()
                == "connected to \"home\" (10:00:00:00:00:01)"
        );
        check!(
            StationState::Failed { reason: Failure::Rejected, retry_in: Some(Duration::from_secs(4)) }.to_string()
                == "failed: the access point refused the connection; retrying in 4 s"
        );
    }
}
