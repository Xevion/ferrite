//! The processor's clock and power on every core: `ferrite_power` driven through this crate's MSR
//! access and MP services.
//!
//! Discovery and the meter run on the boot processor through [`ProbingMsrs`], so a register a model
//! lacks reads as absent instead of faulting. A request runs on every core, since levels differ per
//! core on hybrid and preferred-core parts; the APs use plain MSR access to the registers the boot
//! processor found. What each core's request replaced is kept so [`Power::restore`] can put it back
//! before the next image runs.

use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;

use ferrite_hal::Instant;
use ferrite_power::control::{Applied, ApplyError, Interface, Request, Saved};
use ferrite_power::discover::Platform;
use ferrite_power::telemetry::{Meter, Sample};

use crate::mp::{ApTask, Mp, ProcessorNumber, StackUse};
use crate::x86::{Cpuid, ProbingMsrs, RawMsrs};

/// How long an AP may take to write a few MSRs.
const AP_TIMEOUT: Duration = Duration::from_secs(1);

/// The processor's power registers and what this app changed in them.
pub struct Power {
    platform: Platform,
    msrs: Option<ProbingMsrs>,
    meter: Meter,
    /// Each core's replaced registers, boot processor first.
    saved: Vec<(Option<ProcessorNumber>, Saved)>,
}

impl Power {
    /// Find the registers and log what was found.
    #[must_use]
    pub fn discover() -> Self {
        let mut msrs = ProbingMsrs::new();
        if msrs.is_none() {
            log::warn!("power: the fault-catching MSR access is in use; reading nothing");
        }
        let platform =
            msrs.as_mut().map_or_else(|| Platform::discover(&Cpuid, &mut Absent), |m| Platform::discover(&Cpuid, m));
        log::info!("power: {platform}");
        let meter = Meter::new(&platform);
        Self { platform, msrs, meter, saved: Vec::new() }
    }

    #[must_use]
    pub const fn platform(&self) -> &Platform {
        &self.platform
    }

    /// Ask every core for `request`: the boot processor here, each AP of `mp` through a short task.
    /// Returns how many cores took it; replaces any earlier request, keeping the firmware's values
    /// to restore.
    pub fn apply(&mut self, request: Request, mp: Option<&Mp>) -> usize {
        let Ok(interface) = self.platform.control else { return 0 };
        let bsp = match self.msrs.as_mut() {
            Some(msrs) => interface.apply(request, &Cpuid, msrs),
            None => return 0,
        };
        let mut results = alloc::vec![(None, bsp)];
        if let Some(mp) = mp {
            let aps: Vec<ProcessorNumber> = mp.aps().map(|p| p.number).collect();
            let tasks: Vec<OnCore<Apply>> = aps.iter().map(|_| OnCore::new(Apply { interface, request })).collect();
            mp.scope(|s| {
                for (&number, task) in aps.iter().zip(&tasks) {
                    if let Err(e) = s.start(number, task, AP_TIMEOUT) {
                        log::warn!("power: asking {number} for {request}: {e}");
                    }
                }
            });
            results.extend(aps.into_iter().zip(tasks).map(|(n, t)| (Some(n), t.take().unwrap_or(Err(NOT_RUN)))));
        }
        report(request, &results);
        let first = self.saved.is_empty();
        let mut took = 0;
        for (core, result) in results {
            let Ok(applied) = result else { continue };
            took += usize::from(applied.took());
            if first {
                self.saved.push((core, applied.saved));
            }
        }
        took
    }
}

/// Log what each core took.
fn report(request: Request, results: &[(Option<ProcessorNumber>, Result<Applied, ApplyError>)]) {
    let took = results.iter().filter(|(_, r)| r.as_ref().is_ok_and(Applied::took)).count();
    match results.first() {
        Some((_, Ok(bsp))) => match bsp.levels {
            Some(levels) => log::info!(
                "power: asked {} cores for {request}; {took} took it; boot processor levels {levels}, request {:#x}",
                results.len(),
                bsp.written
            ),
            None => log::info!(
                "power: asked {} cores for {request}; {took} took it; request {:#x}",
                results.len(),
                bsp.written
            ),
        },
        Some((_, Err(e))) => log::warn!("power: the boot processor did not take {request}: {e}"),
        None => {},
    }
    for (core, result) in results.iter().skip(1) {
        match (core, result) {
            (Some(n), Err(e)) => log::warn!("power: {n} did not take the request: {e}"),
            (Some(n), Ok(a)) if !a.took() => {
                log::warn!("power: {n} reads back {:#x} after {:#x}", a.readback, a.written);
            },
            _ => {},
        }
    }
}

impl Power {
    /// Put back every register a request replaced, on every core. HWP and CPPC stay enabled, since
    /// nothing but a reset turns them off.
    pub fn restore(&mut self, mp: Option<&Mp>) {
        let saved = core::mem::take(&mut self.saved);
        let mut on_aps = Vec::new();
        for (core, saved) in saved {
            match core {
                None => {
                    if let Some(msrs) = self.msrs.as_mut()
                        && let Err(e) = saved.restore(msrs)
                    {
                        log::warn!("power: restoring the boot processor: {e}");
                    }
                },
                Some(number) => on_aps.push((number, OnCore::new(Restore(saved)))),
            }
        }
        if let Some(mp) = mp.filter(|_| !on_aps.is_empty()) {
            mp.scope(|s| {
                for (number, task) in &on_aps {
                    if let Err(e) = s.start(*number, task, AP_TIMEOUT) {
                        log::warn!("power: restoring {number}: {e}");
                    }
                }
            });
        }
        log::info!("power: restored the firmware's clock request on {} cores", on_aps.len() + 1);
    }

    /// Read the meter at `now`; `None` on the first call, which starts the interval.
    pub fn sample(&mut self, now: Instant) -> Option<Sample> {
        if self.meter.is_empty() {
            return None;
        }
        self.meter.sample(self.msrs.as_mut()?, now)
    }

    #[must_use]
    pub const fn meter(&self) -> &Meter {
        &self.meter
    }
}

const NOT_RUN: ApplyError = ApplyError::Msr { source: ferrite_power::msr::Fault { address: 0 } };

/// MSR access when there is none: every register reads as absent.
struct Absent;

impl ferrite_power::msr::MsrIo for Absent {
    fn read_raw(&mut self, address: u32) -> Result<u64, ferrite_power::msr::Fault> {
        Err(ferrite_power::msr::Fault { address })
    }

    fn write_raw(&mut self, address: u32, _value: u64) -> Result<(), ferrite_power::msr::Fault> {
        Err(ferrite_power::msr::Fault { address })
    }
}

/// Work one AP does on its own MSRs.
trait CoreWork: Sync {
    type Output: Send;
    fn run(&self, msrs: &mut RawMsrs) -> Self::Output;
}

struct Apply {
    interface: Interface,
    request: Request,
}

impl CoreWork for Apply {
    type Output = Result<Applied, ApplyError>;

    fn run(&self, msrs: &mut RawMsrs) -> Self::Output {
        self.interface.apply(self.request, &Cpuid, msrs)
    }
}

struct Restore(Saved);

impl CoreWork for Restore {
    type Output = ();

    fn run(&self, msrs: &mut RawMsrs) {
        // RawMsrs reports no faults, so this always succeeds.
        let _ = self.0.restore(msrs);
    }
}

/// A [`CoreWork`] and the slot its one AP fills.
struct OnCore<W: CoreWork> {
    work: W,
    output: UnsafeCell<Option<W::Output>>,
    done: AtomicBool,
}

impl<W: CoreWork> OnCore<W> {
    const fn new(work: W) -> Self {
        Self { work, output: UnsafeCell::new(None), done: AtomicBool::new(false) }
    }

    fn take(self) -> Option<W::Output> {
        if self.done.load(Ordering::Acquire) { self.output.into_inner() } else { None }
    }
}

// SAFETY: one AP writes `output` before the Release store of `done`; it is read only through `take`,
// which owns the task after the scope ended and checks `done` with Acquire.
unsafe impl<W: CoreWork> Sync for OnCore<W> {}

// SAFETY: the work touches only the clock and power MSRs discovery found on the boot processor,
// which every core of the package has, and the task's own slot.
unsafe impl<W: CoreWork> ApTask for OnCore<W> {
    fn run(&self) {
        // SAFETY: see the impl's comment.
        let mut msrs = unsafe { RawMsrs::new() };
        let output = self.work.run(&mut msrs);
        // SAFETY: only this AP runs this task, once; nothing reads the slot before `done`.
        unsafe { *self.output.get() = Some(output) };
        self.done.store(true, Ordering::Release);
    }

    fn finish(&self, _stack: StackUse) {}
}
