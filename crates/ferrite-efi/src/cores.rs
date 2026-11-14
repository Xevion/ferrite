//! The processors a split test runs on, through MP Services.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use core::num::NonZeroUsize;

use crate::clock::Clock;
use crate::mp::{self, ControlRegisters, Mp, Processor, ProcessorNumber};

/// Which logical processors split tests run on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoreChoice {
    /// Every enabled logical processor.
    All,
    /// One thread of each physical core.
    Physical,
    /// The BSP and the first APs up to this many in all, every core's first thread before any second.
    Count(NonZeroUsize),
}

/// The processors split tests run on, through MP Services.
pub struct Cores {
    mp: Mp,
    clock: Clock,
    /// The BSP, then the APs in use, every core's first thread before any second thread.
    cpus: Vec<ProcessorNumber>,
    /// Each AP's CR4 and XCR0 before anything ran there.
    before: Vec<(ProcessorNumber, Option<ControlRegisters>)>,
    deepest_stack: usize,
}

impl fmt::Debug for Cores {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cores").field("cpus", &self.cpus).finish_non_exhaustive()
    }
}

impl Cores {
    /// MP Services and the processors `choice` picks; `None` without the protocol.
    pub fn locate(choice: CoreChoice, clock: Clock) -> Option<Self> {
        let mp = Mp::locate().inspect_err(|e| log::warn!("mp: {e}; the sweep runs on the BSP alone")).ok()?;
        for p in mp.processors() {
            log::info!("mp: {p}");
        }
        let cpus = choose(mp.processors(), choice);
        log::info!("sweep: split tests on {} of {} logical processors ({choice:?})", cpus.len(), mp.processors().len());
        let before = mp.ap_control_registers();
        Some(Self { mp, clock, cpus, before, deepest_stack: 0 })
    }

    /// Run split tests on the processors `choice` picks from the next test on.
    pub fn rechoose(&mut self, choice: CoreChoice) {
        self.cpus = choose(self.mp.processors(), choice);
        log::info!("sweep: split tests on {} logical processors ({choice:?})", self.cpus.len());
    }

    #[must_use]
    pub const fn mp(&self) -> &Mp {
        &self.mp
    }

    #[must_use]
    pub const fn clock(&self) -> Clock {
        self.clock
    }

    /// The processors in use: the BSP first, then the APs.
    #[must_use]
    pub fn cpus(&self) -> &[ProcessorNumber] {
        &self.cpus
    }

    /// What the firmware reported of each processor in use, in the same order as [`Cores::cpus`].
    pub fn processors(&self) -> impl Iterator<Item = &Processor> {
        let processors = self.mp.processors();
        self.cpus.iter().filter_map(|&n| processors.iter().find(|p| p.number == n))
    }

    /// The number of the first processor in use, the BSP; 0 with none.
    #[must_use]
    pub fn bsp(&self) -> usize {
        self.cpus.first().map_or(0, |p| p.get())
    }

    /// Stop using `lost`.
    pub fn drop_cpus(&mut self, lost: &[ProcessorNumber]) {
        self.cpus.retain(|c| !lost.contains(c));
    }

    /// Stop using every processor but the first, the BSP.
    pub fn keep_bsp_only(&mut self) {
        self.cpus.truncate(1);
    }

    /// Note an AP's stack depth; whether it is the deepest so far.
    pub fn note_stack(&mut self, bytes: usize) -> bool {
        let deeper = bytes > self.deepest_stack;
        if deeper {
            self.deepest_stack = bytes;
        }
        deeper
    }

    /// Log each AP's CR4 and XCR0 against what they were before anything ran there.
    pub fn check_restored(&self) {
        let show =
            |r: &Option<ControlRegisters>| r.map_or_else(|| String::from("no answer"), |r| alloc::format!("{r}"));
        let after = self.mp.ap_control_registers();
        for ((cpu, was), (_, now)) in self.before.iter().zip(&after) {
            if was == now {
                log::info!("mp: {cpu} {} before and after", show(was));
            } else {
                log::warn!("mp: {cpu} {} before, {} after", show(was), show(now));
            }
        }
        if mp::ap_panics() > 0 {
            log::error!("mp: {} AP panics", mp::ap_panics());
        }
    }
}

/// The BSP, then the enabled, healthy APs `choice` keeps, every core's first thread before any second.
fn choose(processors: &[Processor], choice: CoreChoice) -> Vec<ProcessorNumber> {
    let mut order: Vec<&Processor> = processors.iter().filter(|p| p.bsp).collect();
    let mut aps: Vec<&Processor> = processors.iter().filter(|p| p.enabled && p.healthy && !p.bsp).collect();
    aps.sort_by_key(|p| (p.location.thread, p.location.package, p.location.core, p.number));
    order.extend(aps);
    let keep = match choice {
        CoreChoice::All => order.len(),
        CoreChoice::Physical => {
            order.retain(|p| p.bsp || p.location.thread == 0);
            order.len()
        },
        CoreChoice::Count(n) => n.get(),
    };
    order.into_iter().take(keep).map(|p| p.number).collect()
}
