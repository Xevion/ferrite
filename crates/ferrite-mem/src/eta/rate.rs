//! The live rate of the running test.

use alloc::collections::VecDeque;
use core::time::Duration;

use ferrite_hal::Instant;

/// Bytes per second over the last few seconds of work, from a running count of words.
///
/// A sliding window rather than the average since the start, so it follows a test whose speed
/// changes (a sweep that moves from cache-sized to memory-sized strides) and forgets a stall.
#[derive(Clone, Debug, Default)]
pub struct RateMeter {
    samples: VecDeque<(Instant, u64)>,
}

impl RateMeter {
    /// How far back the rate looks.
    pub const WINDOW: Duration = Duration::from_secs(3);
    /// The least time of work a rate is reported over, so the first chunk does not read as a spike.
    pub const MIN_SPAN: Duration = Duration::from_millis(500);
    /// Samples closer than this to the last are dropped, which bounds the window's length.
    const SPACING: Duration = Duration::from_millis(100);

    #[must_use]
    pub const fn new() -> Self {
        Self { samples: VecDeque::new() }
    }

    /// Forget the window: a new test, or work resuming after a wait.
    pub fn reset(&mut self) {
        self.samples.clear();
    }

    /// Note that `words` of work are done in all at `now`. A count that went down starts over.
    pub fn push(&mut self, now: Instant, words: u64) {
        if let Some(&(at, before)) = self.samples.back() {
            if words < before {
                self.samples.clear();
            } else if now - at < Self::SPACING {
                return;
            }
        }
        self.samples.push_back((now, words));
        while self.samples.len() > 2 && self.samples.get(1).is_some_and(|&(at, _)| now - at >= Self::WINDOW) {
            self.samples.pop_front();
        }
    }

    /// Bytes of work per second over the window, once it spans [`Self::MIN_SPAN`].
    #[must_use]
    pub fn bytes_per_sec(&self) -> Option<u64> {
        let (&(first_at, first), &(last_at, last)) = (self.samples.front()?, self.samples.back()?);
        let span = last_at - first_at;
        if span < Self::MIN_SPAN {
            return None;
        }
        let bytes = u128::from(last - first) * size_of::<u64>() as u128;
        u64::try_from(bytes * 1_000_000_000 / span.as_nanos()).ok()
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    fn at(ms: u64) -> Instant {
        Instant::from_epoch(Duration::from_millis(ms))
    }

    #[test]
    fn the_rate_follows_the_window_and_waits_for_enough_span() {
        let mut meter = RateMeter::new();
        meter.push(at(0), 0);
        meter.push(at(300), 3_000_000);
        check!(meter.bytes_per_sec().is_none(), "300 ms is too short to call a rate");
        // 10 M words a second for two seconds, then 1 M a second for three.
        for ms in (100..=2000).step_by(100) {
            meter.push(at(ms), ms * 10_000);
        }
        check!(meter.bytes_per_sec() == Some(80_000_000));
        for ms in (2100..=5000).step_by(100) {
            meter.push(at(ms), 20_000_000 + (ms - 2000) * 1_000);
        }
        check!(meter.bytes_per_sec() == Some(8_000_000), "the fast start has left the window");
    }

    #[test]
    fn a_count_that_goes_back_starts_the_window_again() {
        let mut meter = RateMeter::new();
        meter.push(at(0), 1_000);
        meter.push(at(1000), 2_000);
        check!(meter.bytes_per_sec().is_some());
        meter.push(at(1100), 0);
        check!(meter.bytes_per_sec().is_none());
    }
}
