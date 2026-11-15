//! Power, throttling and limit reasons over time, read on the boot processor.

use core::time::Duration;

use ferrite_hal::Instant;

use crate::discover::{Energy, Platform};
use crate::limits::{Layout, Limit, Reasons};
use crate::msr::MsrIo;
use crate::rapl::{self, Counter, Domain};
use crate::units::{MicroJoules, MilliWatts};

const DOMAINS: usize = <Domain as strum::VariantArray>::VARIANTS.len();
const LIMITS: usize = <Limit as strum::VariantArray>::VARIANTS.len();

/// The average power of each domain over one interval.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Power([Option<MilliWatts>; DOMAINS]);

impl Power {
    #[must_use]
    pub fn of(&self, domain: Domain) -> Option<MilliWatts> {
        self.0[domain as usize]
    }

    pub fn iter(&self) -> impl Iterator<Item = (Domain, MilliWatts)> + '_ {
        <Domain as strum::VariantArray>::VARIANTS.iter().filter_map(|&d| self.of(d).map(|p| (d, p)))
    }
}

/// One reading over the interval since the previous one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    pub interval: Duration,
    pub power: Power,
    /// Time RAPL held the package back, Intel only.
    pub throttled: Option<Duration>,
    pub reasons: Option<Reasons>,
}

/// How often each limit was clipping the clock across the samples taken.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    pub samples: u32,
    active: [u32; LIMITS],
}

impl Tally {
    pub fn add(&mut self, reasons: Reasons) {
        self.samples += 1;
        for limit in reasons.active.iter() {
            self.active[limit as usize] += 1;
        }
    }

    /// The samples `limit` was active in.
    #[must_use]
    pub const fn count(&self, limit: Limit) -> u32 {
        self.active[limit as usize]
    }

    /// Each limit seen, most often first, with its count.
    pub fn ranked(&self) -> impl Iterator<Item = (Limit, u32)> {
        let mut seen: [(Limit, u32); LIMITS] =
            core::array::from_fn(|i| (<Limit as strum::VariantArray>::VARIANTS[i], self.active[i]));
        seen.sort_unstable_by_key(|&(_, n)| core::cmp::Reverse(n));
        seen.into_iter().filter(|&(_, n)| n > 0)
    }

    #[must_use]
    pub fn merged(mut self, other: &Self) -> Self {
        self.samples += other.samples;
        self.active.iter_mut().zip(other.active).for_each(|(a, b)| *a += b);
        self
    }
}

/// Energy counters, throttle time and limit reasons, read together.
#[derive(Clone, Debug)]
pub struct Meter {
    energy: Option<Energy>,
    counters: [Counter; DOMAINS],
    throttle: Option<Counter>,
    limits: Option<Layout>,
    last: Option<Instant>,
    tally: Tally,
}

impl Meter {
    #[must_use]
    pub fn new(platform: &Platform) -> Self {
        let intel_rapl = platform.energy.is_some_and(|e| !e.amd);
        Self {
            energy: platform.energy,
            counters: [Counter::default(); DOMAINS],
            throttle: intel_rapl.then(Counter::default),
            limits: platform.limit_reasons,
            last: None,
            tally: Tally::default(),
        }
    }

    /// Whether there is anything to measure.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.energy.is_none() && self.limits.is_none()
    }

    /// Read everything at `now`; `None` for the first call, which only starts the interval.
    pub fn sample(&mut self, msr: &mut impl MsrIo, now: Instant) -> Option<Sample> {
        let mut power = Power::default();
        let mut throttled = None;
        let interval = self.last.map(|last| now.saturating_duration_since(last));
        if let Some(energy) = self.energy {
            for domain in energy.domains.iter() {
                let Some(raw) = domain.counter(energy.amd).and_then(|m| msr.read(m).ok()) else { continue };
                let counts = self.counters[domain as usize].update(raw);
                power.0[domain as usize] =
                    interval.and_then(|over| energy.units.energy(domain).to_micro(counts).per(over));
            }
            if let Some(counter) = self.throttle.as_mut()
                && let Ok(raw) = msr.read(rapl::PKG_PERF_STATUS)
            {
                let counts = counter.update(raw);
                throttled = interval.map(|_| energy.units.time(counts));
            }
        }
        let reasons = self.limits.and_then(|layout| msr.read_raw(layout.address()).ok().map(|raw| layout.decode(raw)));
        self.last = Some(now);
        let interval = interval?;
        if let Some(reasons) = reasons {
            self.tally.add(reasons);
        }
        Some(Sample { interval, power, throttled, reasons })
    }

    /// The energy a domain used since the first sample.
    #[must_use]
    pub fn energy(&self, domain: Domain) -> Option<MicroJoules> {
        let energy = self.energy.filter(|e| e.domains.has(domain))?;
        Some(energy.units.energy(domain).to_micro(self.counters[domain as usize].total()))
    }

    #[must_use]
    pub const fn tally(&self) -> &Tally {
        &self.tally
    }
}
