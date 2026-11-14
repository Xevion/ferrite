//! The queue of notifications waiting to go out, and what a failed attempt does to it: a bounded
//! queue that drops its oldest entry, a retry with a doubling backoff, and a limit on attempts.
//!
//! Nothing here touches a socket, so the whole policy is testable with plain instants.

use alloc::collections::VecDeque;
use core::time::Duration;

use ferrite_hal::Instant;

use super::request::Notification;

/// How patient the queue is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    /// Notifications held, the one being sent included.
    pub capacity: usize,
    /// How long one attempt (lookup, connect, send, reply) may take.
    pub attempt_timeout: Duration,
    /// The wait after the first failed attempt; each further failure doubles it.
    pub retry_base: Duration,
    pub retry_max: Duration,
    /// Attempts before a notification is given up on.
    pub max_attempts: u32,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            capacity: 8,
            attempt_timeout: Duration::from_secs(10),
            retry_base: Duration::from_secs(1),
            retry_max: Duration::from_secs(30),
            max_attempts: 5,
        }
    }
}

impl Policy {
    /// The wait after the `failures`-th failure.
    fn backoff(&self, failures: u32) -> Duration {
        let doublings = failures.saturating_sub(1).min(16);
        self.retry_base.saturating_mul(1 << doublings).min(self.retry_max)
    }
}

/// How an attempt ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Delivered,
    /// Failed in a way another try may fix.
    Retry,
    /// Refused for good.
    Rejected,
}

/// What became of the notification an attempt was for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Settled {
    Delivered,
    /// Back in the queue, to be tried again after `after`; `attempt` is the one that just failed.
    Retrying {
        attempt: u32,
        after: Duration,
    },
    /// Dropped after `attempts` failures.
    GaveUp {
        attempts: u32,
    },
    Rejected,
}

struct Entry {
    notification: Notification,
    failures: u32,
    not_before: Instant,
}

pub struct Outbox {
    policy: Policy,
    /// Oldest first. The front is the one being sent while `in_flight`.
    queue: VecDeque<Entry>,
    in_flight: bool,
    dropped: u64,
}

impl Outbox {
    pub fn new(policy: Policy) -> Self {
        Self { policy, queue: VecDeque::with_capacity(policy.capacity), in_flight: false, dropped: 0 }
    }

    pub const fn policy(&self) -> &Policy {
        &self.policy
    }

    /// Notifications held, the one being sent included.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub const fn in_flight(&self) -> bool {
        self.in_flight
    }

    /// Notifications dropped for lack of room.
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Queue `notification`. When the queue is full its oldest entry that is not being sent is
    /// dropped and returned (the new one itself when nothing else can go).
    pub fn push(&mut self, notification: Notification) -> Option<Notification> {
        let entry = Entry { notification, failures: 0, not_before: Instant::default() };
        let mut evicted = None;
        if self.queue.len() >= self.policy.capacity.max(1) {
            evicted = match self.queue.remove(usize::from(self.in_flight)) {
                Some(old) => Some(old.notification),
                None => return Some(entry.notification),
            };
            self.dropped += 1;
        }
        self.queue.push_back(entry);
        evicted
    }

    /// Start sending the oldest notification if none is being sent and its retry wait is over.
    pub fn begin(&mut self, now: Instant) -> Option<&Notification> {
        if self.in_flight {
            return None;
        }
        let front = self.queue.front().filter(|entry| now >= entry.not_before)?;
        self.in_flight = true;
        Some(&front.notification)
    }

    /// Record how the attempt `begin` started ended; `None` when none was running.
    pub fn settle(&mut self, outcome: Outcome, now: Instant) -> Option<Settled> {
        if !self.in_flight {
            return None;
        }
        self.in_flight = false;
        match outcome {
            Outcome::Delivered => {
                self.queue.pop_front();
                Some(Settled::Delivered)
            },
            Outcome::Rejected => {
                self.queue.pop_front();
                Some(Settled::Rejected)
            },
            Outcome::Retry => {
                let entry = self.queue.front_mut()?;
                entry.failures += 1;
                let attempts = entry.failures;
                if attempts >= self.policy.max_attempts {
                    self.queue.pop_front();
                    return Some(Settled::GaveUp { attempts });
                }
                let after = self.policy.backoff(attempts);
                entry.not_before = now + after;
                Some(Settled::Retrying { attempt: attempts, after })
            },
        }
    }

    /// The attempt in progress was lost with the port it ran on: no fault of the notification's, so
    /// it goes again at once and is not counted as a failure.
    pub fn cancel(&mut self) {
        self.in_flight = false;
    }
}

#[cfg(test)]
mod tests {
    use alloc::format;
    use alloc::vec::Vec;

    use assert2::{assert, check};

    use super::*;
    use crate::notify::Event;

    fn at(ms: u64) -> Instant {
        Instant::from_epoch(Duration::from_millis(ms))
    }

    fn note(n: usize) -> Notification {
        Notification::new(Event::Pass, "t", format!("{n}"))
    }

    fn message(outbox: &mut Outbox, now: Instant) -> Option<alloc::string::String> {
        outbox.begin(now).map(|n| n.message.clone())
    }

    #[test]
    fn a_full_queue_drops_the_oldest_and_counts_it() {
        let mut outbox = Outbox::new(Policy { capacity: 3, ..Policy::default() });
        check!((0..3).all(|n| outbox.push(note(n)).is_none()));
        check!(outbox.push(note(3)) == Some(note(0)));
        check!(outbox.push(note(4)) == Some(note(1)));
        check!(outbox.dropped() == 2);
        check!(outbox.len() == 3);
        let mut sent = Vec::new();
        while let Some(m) = message(&mut outbox, at(0)) {
            sent.push(m);
            outbox.settle(Outcome::Delivered, at(0));
        }
        check!(sent == ["2", "3", "4"]);
        check!(outbox.is_empty());
    }

    #[test]
    fn the_notification_being_sent_is_not_the_one_dropped() {
        let mut outbox = Outbox::new(Policy { capacity: 2, ..Policy::default() });
        outbox.push(note(0));
        outbox.push(note(1));
        check!(message(&mut outbox, at(0)).as_deref() == Some("0"));
        check!(outbox.push(note(2)) == Some(note(1)));
        check!(outbox.settle(Outcome::Delivered, at(1)) == Some(Settled::Delivered));
        check!(message(&mut outbox, at(1)).as_deref() == Some("2"));

        let mut one = Outbox::new(Policy { capacity: 1, ..Policy::default() });
        one.push(note(0));
        one.begin(at(0));
        check!(one.push(note(1)) == Some(note(1)), "nothing else can go, so the newcomer does");
        check!(one.len() == 1);
    }

    #[test]
    fn one_attempt_runs_at_a_time() {
        let mut outbox = Outbox::new(Policy::default());
        outbox.push(note(0));
        outbox.push(note(1));
        check!(outbox.begin(at(0)).is_some());
        check!(outbox.begin(at(0)).is_none());
        check!(outbox.settle(Outcome::Delivered, at(0)).is_some());
        check!(outbox.settle(Outcome::Delivered, at(0)).is_none(), "nothing was running");
    }

    #[test]
    fn retries_wait_longer_each_time_then_give_up() {
        let policy = Policy { max_attempts: 5, ..Policy::default() };
        let mut outbox = Outbox::new(policy);
        outbox.push(note(0));
        let mut now = at(0);
        let mut waits = Vec::new();
        loop {
            assert!(let Some(_) = outbox.begin(now));
            match outbox.settle(Outcome::Retry, now) {
                Some(Settled::Retrying { after, .. }) => {
                    waits.push(after);
                    check!(outbox.begin(now + after.saturating_sub(Duration::from_millis(1))).is_none(), "too early");
                    now = now + after;
                },
                Some(Settled::GaveUp { attempts }) => {
                    check!(attempts == 5);
                    break;
                },
                other => panic!("{other:?}"),
            }
        }
        check!(waits == [1, 2, 4, 8].map(Duration::from_secs));
        check!(outbox.is_empty());
    }

    #[test]
    fn the_wait_is_capped() {
        let policy = Policy::default();
        check!(policy.backoff(5) == Duration::from_secs(16));
        check!(policy.backoff(6) == Duration::from_secs(30));
        check!(policy.backoff(1000) == Duration::from_secs(30));
    }

    #[test]
    fn a_failing_head_holds_the_queue_in_order() {
        let mut outbox = Outbox::new(Policy::default());
        outbox.push(note(0));
        outbox.push(note(1));
        outbox.begin(at(0));
        outbox.settle(Outcome::Retry, at(0));
        check!(message(&mut outbox, at(500)).is_none());
        check!(message(&mut outbox, at(1_000)).as_deref() == Some("0"));
    }

    #[test]
    fn a_refusal_is_final_and_a_lost_attempt_is_free() {
        let mut outbox = Outbox::new(Policy::default());
        outbox.push(note(0));
        outbox.push(note(1));
        outbox.begin(at(0));
        check!(outbox.settle(Outcome::Rejected, at(0)) == Some(Settled::Rejected));
        outbox.begin(at(0));
        outbox.cancel();
        check!(message(&mut outbox, at(0)).as_deref() == Some("1"), "straight away, no wait");
        check!(
            outbox.settle(Outcome::Retry, at(0))
                == Some(Settled::Retrying { attempt: 1, after: Duration::from_secs(1) })
        );
    }
}
