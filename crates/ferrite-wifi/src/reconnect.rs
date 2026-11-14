use core::time::Duration;

/// How long to wait between attempts, and when a connection counts as having worked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    /// The wait after the first failure; each further one doubles it.
    pub initial: Duration,
    pub max: Duration,
    /// A connection that stayed up this long resets the backoff, so a link that drops once an hour
    /// reconnects at once while one that drops every few seconds backs off.
    pub stable: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Self { initial: Duration::from_secs(1), max: Duration::from_secs(30), stable: Duration::from_secs(10) }
    }
}

/// Exponential backoff over consecutive failures.
#[derive(Clone, Copy, Debug)]
pub struct Reconnect {
    policy: Policy,
    failures: u32,
}

impl Reconnect {
    pub const fn new(policy: Policy) -> Self {
        Self { policy, failures: 0 }
    }

    /// Failures since the last stable connection.
    pub const fn failures(&self) -> u32 {
        self.failures
    }

    /// Count a failure and return how long to wait before the next attempt.
    pub fn next_delay(&mut self) -> Duration {
        let delay = self
            .policy
            .initial
            .checked_mul(1_u32.checked_shl(self.failures).unwrap_or(u32::MAX))
            .map_or(self.policy.max, |delay| delay.min(self.policy.max));
        self.failures = self.failures.saturating_add(1);
        delay
    }

    /// A connection that was up for `held` has ended.
    pub fn connection_ended(&mut self, held: Duration) {
        if held >= self.policy.stable {
            self.failures = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;
    use rstest::rstest;

    use super::*;

    const fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn delays_double_to_the_cap_and_stay_there() {
        let mut backoff = Reconnect::new(Policy::default());
        let delays: [Duration; 8] = core::array::from_fn(|_| backoff.next_delay());
        check!(delays == [secs(1), secs(2), secs(4), secs(8), secs(16), secs(30), secs(30), secs(30)]);
        check!(backoff.failures() == 8);
    }

    #[test]
    fn a_huge_failure_count_cannot_overflow_the_shift() {
        let mut backoff = Reconnect::new(Policy::default());
        for _ in 0..200 {
            check!(backoff.next_delay() <= secs(30));
        }
    }

    #[rstest]
    #[case::flapping_keeps_the_delay(secs(3), secs(8))]
    #[case::just_short_of_stable(Duration::from_millis(9_999), secs(8))]
    #[case::stable_resets(secs(10), secs(1))]
    #[case::long_resets(secs(3600), secs(1))]
    fn only_a_stable_connection_resets_the_backoff(#[case] held: Duration, #[case] next: Duration) {
        let mut backoff = Reconnect::new(Policy::default());
        for _ in 0..3 {
            backoff.next_delay();
        }
        backoff.connection_ended(held);
        check!(backoff.next_delay() == next);
    }
}
