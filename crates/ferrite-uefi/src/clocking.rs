//! The processor's clock while the memory is tested: the `[cpu]` policy, fed by the test the sweep
//! is on, and the power it costs.

use alloc::string::String;
use core::fmt::Write as _;
use core::time::Duration;

use ferrite_efi::mp::Mp;
use ferrite_efi::power::Power;
use ferrite_hal::Instant;
use ferrite_power::control::{Bound, Request};
use ferrite_power::governor::{Governor, Ladder, Observation};
use ferrite_power::policy::{ClockMode, Policy};
use ferrite_power::rapl::Domain;
use ferrite_power::units::MicroJoules;

use crate::sweep::SweepStatus;

/// Ceilings in a probe, from the highest clock down to the base clock.
const LADDER_STEPS: usize = 5;
/// How often the governor and the meter look.
const TICK: Duration = Duration::from_millis(100);
/// How often the power drawn goes to the log.
const LOG_INTERVAL: Duration = Duration::from_secs(30);

/// The clock policy, the processor's registers, and the meter.
pub struct Clocking {
    power: Power,
    governor: Option<Governor>,
    ceiling: Option<Bound>,
    next_tick: Instant,
    /// When the log line was last written, and the package energy then.
    logged: Option<(Instant, MicroJoules)>,
}

impl Clocking {
    /// Find the registers and apply `policy`'s first request on every core.
    pub fn start(policy: Policy, mp: Option<&Mp>) -> Self {
        let mut power = Power::discover();
        log::info!("cpu: {policy}");
        let governor = if policy.clock == ClockMode::Firmware {
            None
        } else {
            power.apply(Request::MAX, mp);
            power.levels().map(|levels| {
                let ladder = Ladder::from_levels(&levels, LADDER_STEPS);
                log::info!("cpu: clock ceilings {ladder}");
                Governor::new(policy, ladder)
            })
        };
        Self { power, governor, ceiling: Some(Bound::Highest), next_tick: Instant::default(), logged: None }
    }

    /// Look at the sweep and the sensors every [`TICK`]: change the ceiling when the policy says,
    /// and read the meter.
    pub fn tick(&mut self, now: Instant, status: &SweepStatus, celsius: Option<i16>) {
        if now < self.next_tick {
            return;
        }
        self.next_tick = now + TICK;
        let sample = self.power.sample(now);
        let energy = self.power.meter().energy(Domain::Package);
        if let Some(governor) = self.governor.as_mut()
            && !status.paused
            && status.bytes > 0
        {
            let obs = Observation { now, test: status.test, done: status.test_done, celsius, energy };
            if let Some(request) = governor.observe(obs) {
                self.ceiling = Some(request.ceiling);
                self.power.publish(request);
            }
        }
        let (Some(energy), Some(sample)) = (energy, sample) else { return };
        match self.logged {
            None => self.logged = Some((now, energy)),
            Some((at, before)) if now.saturating_duration_since(at) >= LOG_INTERVAL => {
                let watts = MicroJoules(energy.0.saturating_sub(before.0)).per(now.saturating_duration_since(at));
                let mut line = String::new();
                if let Some(watts) = watts {
                    let _ = write!(line, " package {watts}");
                }
                if let Some(ceiling) = self.ceiling {
                    let _ = write!(line, ", ceiling {ceiling}");
                }
                if let Some(c) = celsius {
                    let _ = write!(line, ", {c} C");
                }
                if let Some(reasons) = sample.reasons {
                    let _ = write!(line, ", limited by [{}]", reasons.active);
                }
                let _ = write!(line, " during {}", status.test);
                log::info!("power:{line}");
                self.logged = Some((now, energy));
            },
            Some(_) => {},
        }
    }

    /// Log what the run measured and cost, then put the firmware's clock request back.
    pub fn end(&mut self, mp: Option<&Mp>) {
        if let Some(governor) = &self.governor {
            log::info!("cpu: clock ceilings {}", governor.ladder());
            for record in governor.records() {
                log::info!("cpu: {record}");
            }
        }
        let meter = self.power.meter();
        if let Some(energy) = meter.energy(Domain::Package) {
            log::info!("power: the package used {energy} over the run");
        }
        let tally = meter.tally();
        if tally.samples > 0 {
            let mut line = String::new();
            for (limit, count) in tally.ranked() {
                let share = u64::from(count) * 100 / u64::from(tally.samples);
                let _ = write!(line, " {}={share}%", <&str>::from(limit));
            }
            let line = if line.is_empty() { " nothing" } else { line.as_str() };
            log::info!("power: clock limited by{line} over {} samples", tally.samples);
        }
        self.power.restore(mp);
    }
}
