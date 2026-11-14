//! The memory sweep: the `ferrite-mem` suite run over every free range of memory, pass after pass.
//!
//! [`Sweep::step`] runs a bounded chunk of the current test on the BSP so the caller can interleave
//! other work. Tests that split ([`Mode::Split`]) run instead through [`Sweep::run_split`] on every
//! core of [`Cores`] at once, each on its own share, while the BSP keeps its duties through a
//! callback; consecutive split tests share one dispatch of the APs. The memory is whole pages from
//! boot services, one block per free range of the firmware's memory map less a reserve (see
//! [`ranges`]), freed on drop. Steps of the AVX tiers run inside [`VectorState::wide`], in chunks
//! of at most [`WIDE_CHUNK_BYTES`], since the firmware's interrupt entry does not save the upper
//! halves of the vector registers.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;
use core::num::NonZeroUsize;
use core::ptr::NonNull;
use core::time::Duration;

use ferrite_efi::clock::Clock;
use ferrite_efi::cpu::VectorState;
use ferrite_efi::mp::{self, ControlRegisters, Mp, Processor, ProcessorNumber, STACK_PROBE};
use ferrite_hal::{Clock as _, Instant};
use ferrite_mem::analysis::{Classification, FaultStats};
use ferrite_mem::suite::{self, Config, Kernels, Mode, Preset, Progress, Schedule, Span, Suite, Test, TestKind};
use ferrite_mem::{Caps, Fault, Faults, Region, Regions, WIDE_CHUNK_BYTES};
use uefi::boot::{self, AllocateType, MemoryType};
use uefi::mem::memory_map::{MemoryAttribute, MemoryMap, MemoryMapOwned};

use self::ranges::Range;
use crate::crew::{AP_PATIENCE, Crew, Job, Seat, SeatState, Turn};
use crate::report::{self, CpuInfo, FAULT_LOG, FaultRecord, History, Snapshot, TestRecord};

mod ranges;

const PAGE: usize = 4096;
const WORD: usize = size_of::<u64>();
/// Faults handed to the statistics per step; every one is still counted.
const REPORTS_PER_STEP: u64 = 256;
/// The slowest per-core rate an AP's dispatch timeout allows for, in bytes per second.
const AP_FLOOR_RATE: u64 = 250_000_000;
/// How long the BSP waits for every AP of a split run to reach the start.
const GATHER_PATIENCE: Duration = Duration::from_secs(2);
/// Added to every AP's dispatch timeout, for the start and for waiting on slower cores at gates.
const AP_SLACK: Duration = Duration::from_secs(10);
/// A dispatch of the APs takes no new test once it has run this long, so the firmware's timeout on
/// the dispatch (this plus the longest test's) bounds how long a hung AP stalls the sweep.
const DISPATCH_BUDGET: Duration = Duration::from_secs(180);
/// How often the BSP asks the firmware which AP dispatches have ended.
const CHECK_EVERY: Duration = Duration::from_millis(2);

/// What the sweep allocates in one piece when the memory map cannot be had.
const FALLBACK_BYTES: usize = 1 << 30;
/// Smallest buffer worth sweeping when the fallback size cannot be allocated.
pub const MIN_SWEEP_BYTES: usize = 1 << 20;
/// How many times the sweep reads the memory map for ranges its own allocations disturbed.
const CLAIM_ROUNDS: usize = 3;

/// Sweep progress for the UI, the status page and the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SweepStatus {
    pub bytes: usize,
    pub test: &'static str,
    /// The test's 1-based position in the pass, and the number of tests in it.
    pub position: (usize, usize),
    /// Progress through the current test and through the pass, over every core, 0 to 100.
    pub test_percent: u16,
    pub percent: u16,
    pub passes: u64,
    /// The widest kernel tier in use, for display.
    pub tier: &'static str,
    /// How the current test runs, and on how many logical processors.
    pub mode: Mode,
    pub cpus: usize,
    /// Mismatched words over all passes.
    pub faults: u64,
    /// The first fault of the run, with the test and processor that found it.
    pub first_fault: Option<FaultRecord>,
    pub analysis: Classification,
    pub preset: Preset,
    /// Sweeping is paused by request.
    pub paused: bool,
}

impl fmt::Display for SweepStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (i, n) = self.position;
        let mode: &str = self.mode.into();
        write!(
            f,
            "{} ({i}/{n}) {}% pass={} {}% {} {mode} on {} cpus faults={}",
            self.test, self.test_percent, self.passes, self.percent, self.tier, self.cpus, self.faults
        )
    }
}

impl SweepStatus {
    const IDLE: Self = Self {
        bytes: 0,
        test: "none",
        position: (0, 0),
        test_percent: 0,
        percent: 0,
        passes: 0,
        tier: "none",
        mode: Mode::Single,
        cpus: 0,
        faults: 0,
        first_fault: None,
        analysis: Classification::Clean,
        preset: Preset::Standard,
        paused: false,
    };
}

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
    /// Each AP's CR4 and XCR0 before the sweep ran anything there.
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

    fn infos(&self) -> Vec<CpuInfo> {
        let processors = self.mp.processors();
        self.cpus
            .iter()
            .filter_map(|&n| processors.iter().find(|p| p.number == n))
            .map(|p| CpuInfo {
                number: p.number.get(),
                package: p.location.package,
                core: p.location.core,
                thread: p.location.thread,
                bsp: p.bsp,
            })
            .collect()
    }

    fn bsp(&self) -> usize {
        self.cpus.first().map_or(0, |p| p.get())
    }

    /// Log each AP's CR4 and XCR0 against what they were before the sweep.
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

/// One block of memory the sweep owns: boot-services pages, or a host vector in tests.
struct Block {
    base: NonNull<u64>,
    words: usize,
    owner: Owner,
}

enum Owner {
    Pages(usize),
    #[cfg(test)]
    Host {
        _backing: Vec<u64>,
    },
}

impl Block {
    fn pages(base: NonNull<u8>, pages: usize) -> Self {
        Self { base: base.cast(), words: pages * PAGE / WORD, owner: Owner::Pages(pages) }
    }

    #[cfg(test)]
    fn host(words: usize) -> Self {
        let mut backing = alloc::vec![0_u64; words + 8];
        let offset = backing.as_ptr().align_offset(64);
        let base = NonNull::from(&mut backing[offset]);
        Self { base, words, owner: Owner::Host { _backing: backing } }
    }

    const fn bytes(&self) -> usize {
        self.words * WORD
    }
}

impl Drop for Block {
    fn drop(&mut self) {
        match self.owner {
            Owner::Pages(pages) => {
                // SAFETY: allocated with this page count, and the regions over it are gone (see `Run`).
                if let Err(e) = unsafe { boot::free_pages(self.base.cast(), pages) } {
                    log::warn!("sweep: freeing {pages} pages at {:#x} failed: {e}", self.base.addr());
                }
            },
            #[cfg(test)]
            Owner::Host { .. } => {},
        }
    }
}

/// The memory the sweep owns, in address order.
struct Buffer {
    blocks: Vec<Block>,
}

impl Buffer {
    /// One block of up to `bytes` anywhere, for when the memory map cannot be used.
    fn pages(bytes: usize) -> Option<Self> {
        let pages = bytes / PAGE;
        let ptr = boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, pages).ok()?;
        Some(Self { blocks: alloc::vec![Block::pages(ptr, pages)] })
    }

    #[cfg(test)]
    fn host(words: usize) -> Self {
        Self::host_blocks(&[words])
    }

    #[cfg(test)]
    fn host_blocks(words: &[usize]) -> Self {
        Self { blocks: words.iter().map(|&w| Block::host(w)).collect() }
    }

    fn bytes(&self) -> usize {
        self.blocks.iter().map(Block::bytes).sum()
    }
}

/// The free ranges of the memory map that the sweep may use, and the conventional memory it holds
/// in all.
struct FreeMemory {
    ranges: Vec<Range>,
    conventional: u64,
}

impl FreeMemory {
    /// The conventional ranges of `map` that are cacheable and not set aside for a special purpose.
    fn of(map: &MemoryMapOwned) -> Self {
        let mut free = Self { ranges: Vec::new(), conventional: 0 };
        for d in map.entries().filter(|d| d.ty == MemoryType::CONVENTIONAL) {
            free.conventional += d.page_count * PAGE as u64;
            let usable =
                d.att.contains(MemoryAttribute::WRITE_BACK) && !d.att.contains(MemoryAttribute::SPECIAL_PURPOSE);
            if usable {
                free.ranges.push(Range::new(d.phys_start, d.page_count as usize));
            }
        }
        free
    }
}

/// Every free range of the memory map except a reserve, allocated range by range at their own
/// addresses, the largest first. Allocating moves the firmware's own pool, so the map is read again
/// for what a range's allocation could not get; `None` when no range could be taken.
fn claim_free_memory() -> Option<Buffer> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut plan: Option<(u64, u64)> = None;
    for _ in 0..CLAIM_ROUNDS {
        let map =
            boot::memory_map(MemoryType::LOADER_DATA).inspect_err(|e| log::warn!("sweep: no memory map: {e}")).ok()?;
        let free = FreeMemory::of(&map);
        drop(map);
        let (_, reserve) = *plan.get_or_insert_with(|| {
            let reserve = ranges::reserve_for(ranges::eligible_bytes(&free.ranges));
            (free.conventional, reserve)
        });
        let wanted = ranges::select(&free.ranges, reserve);
        let before = blocks.len();
        for range in wanted {
            match boot::allocate_pages(
                AllocateType::Address(range.start.get()),
                MemoryType::LOADER_DATA,
                range.pages.get(),
            ) {
                Ok(ptr) => {
                    log::info!("sweep: range {range}");
                    blocks.push(Block::pages(ptr, range.pages.get()));
                },
                Err(e) => log::debug!("sweep: range {range} not taken: {e}"),
            }
        }
        if blocks.len() == before {
            break;
        }
    }
    blocks.sort_by_key(|b| b.base.addr());
    let buffer = Buffer { blocks };
    let (conventional, reserve) = plan?;
    log::info!(
        "sweep: {} ranges, {} MiB of the {} MiB the memory map reports free; {} MiB reserve, memory below {} MiB untouched",
        buffer.blocks.len(),
        buffer.bytes() >> 20,
        conventional >> 20,
        reserve >> 20,
        ranges::LOW_MEMORY >> 20
    );
    (!buffer.blocks.is_empty()).then_some(buffer)
}

/// The suite over its buffer. Fields drop in order, so the regions go before the memory under them.
struct Run {
    suite: Suite<'static>,
    _buffer: Buffer,
}

impl Run {
    /// The suite over `buffer`; `None`, freeing the buffer, when it cannot be regions.
    fn new(buffer: Buffer, kernels: Kernels, config: Config, preset: Preset) -> Option<Self> {
        let made: Result<Vec<Region<'static>>, _> = buffer
            .blocks
            .iter()
            // SAFETY: each block is ours alone and outlives the suite (field order in `Run`).
            .map(|b| unsafe { Region::from_raw(b.base.as_ptr(), b.words) })
            .collect();
        match made {
            Ok(regions) => {
                let regions = Regions::new(regions);
                log::info!(
                    "sweep: {} bytes in {} regions, kernels {kernels}, {preset:?}",
                    regions.bytes(),
                    regions.layout().spans().len()
                );
                Some(Self { suite: Suite::new(regions, kernels, config, preset), _buffer: buffer })
            },
            Err(e) => {
                log::error!("sweep: unusable buffer: {e}");
                None
            },
        }
    }
}

#[derive(Default)]
struct Tally {
    faults: u64,
    first: Option<FaultRecord>,
    /// The latest faults kept in full, newest last.
    recent: VecDeque<FaultRecord>,
    stats: FaultStats,
}

impl Tally {
    /// Feed a fault kept in full into the statistics and the log, remembering the first one.
    fn record(&mut self, fault: Fault, test: &'static str, cpu: usize, pass: u64) {
        self.stats.record(&fault);
        let found = FaultRecord { fault, test, cpu, pass };
        self.first.get_or_insert(found);
        report::push_bounded(&mut self.recent, found, FAULT_LOG);
    }
}

/// The test being timed for its log line, and when it started.
#[derive(Clone, Copy)]
struct Current {
    test: &'static str,
    pass: u64,
    started: Instant,
    work_bytes: u64,
    faults_at_start: u64,
}

/// Whether the sweep goes on after [`Sweep::run_split`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Stop,
}

/// The BSP's duties during a split test: given the time and the sweep's status, it returns when
/// it is next due, or `None` to stop the sweep.
pub type Duties<'d> = dyn FnMut(Instant, &Snapshot<'_>) -> Option<Instant> + 'd;

pub struct Sweep {
    run: Option<Run>,
    tally: Tally,
    /// The suite has nothing to do before this.
    waiting: Option<Instant>,
    current: Option<Current>,
    cores: Option<Cores>,
    history: History,
    paused: bool,
}

impl Sweep {
    /// Take every free range of memory but a reserve, and run the standard preset over it with the
    /// kernels `caps` allows, split tests on `cores` when given. Without a usable memory map the
    /// sweep takes one block of up to [`FALLBACK_BYTES`], halving down to [`MIN_SWEEP_BYTES`].
    /// With no memory the sweep has no work.
    #[must_use]
    pub fn new(caps: Caps, cores: Option<Cores>) -> Self {
        let buffer = claim_free_memory().or_else(|| alloc_largest(FALLBACK_BYTES, MIN_SWEEP_BYTES, Buffer::pages));
        let preset = Preset::Standard;
        Self::with_buffer(buffer, Kernels::for_caps(caps), preset.config(), preset, cores)
    }

    fn with_buffer(
        buffer: Option<Buffer>,
        kernels: Kernels,
        config: Config,
        preset: Preset,
        cores: Option<Cores>,
    ) -> Self {
        let run = buffer.and_then(|buffer| Run::new(buffer, kernels, config, preset));
        let mut history = History::default();
        history.set_cpus(cores.as_ref().map_or_default(Cores::infos));
        Self { run, tally: Tally::default(), waiting: None, current: None, cores, history, paused: false }
    }

    /// Whether a step now would do work.
    pub fn has_work(&self, now: Instant) -> bool {
        self.run.is_some() && !self.paused && self.waiting.is_none_or(|until| now >= until)
    }

    pub const fn paused(&self) -> bool {
        self.paused
    }

    /// Stop sweeping until [`Sweep::resume`]. A test part-way through on this core carries on from
    /// where it was; a split test is only ever paused between tests, and starts over.
    pub fn pause(&mut self) {
        if !self.paused {
            log::info!("sweep: paused in {}", self.status());
            self.paused = true;
        }
    }

    pub fn resume(&mut self) {
        if self.paused {
            log::info!("sweep: resumed");
            self.paused = false;
        }
    }

    /// Throw away every result and start again from pass 0 with `preset`, over the same buffer.
    pub fn restart(&mut self, preset: Preset) {
        log::info!("sweep: restarting with {preset:?} after {}", self.status());
        if let Some(Run { suite, _buffer: buffer }) = self.run.take() {
            let kernels = suite.kernels();
            self.run = Run::new(buffer, kernels, preset.config(), preset);
        }
        self.tally = Tally::default();
        self.waiting = None;
        self.current = None;
        self.history.reset();
    }

    /// Run split tests on the processors `choice` picks from the next test on.
    pub fn set_cores(&mut self, choice: CoreChoice) {
        if let Some(cores) = self.cores.as_mut() {
            cores.rechoose(choice);
            self.history.set_cpus(cores.infos());
        }
    }

    /// When the suite next has work, if it is waiting.
    pub const fn waiting_until(&self) -> Option<Instant> {
        self.waiting
    }

    /// Logical processors `kind` would run on now.
    fn cpus_for(&self, kind: TestKind) -> usize {
        match (kind.mode(), &self.cores) {
            (Mode::Split, Some(cores)) => cores.cpus.len().max(1),
            _ => 1,
        }
    }

    /// Whether the current test is to run through [`Sweep::run_split`] rather than [`Sweep::step`].
    pub fn split_ready(&self, now: Instant) -> bool {
        let Some(run) = self.run.as_ref() else { return false };
        let test = run.suite.test();
        self.has_work(now) && self.cpus_for(test.kind()) > 1 && test.phase() == 0 && test.done() == 0
    }

    /// Run up to `chunk_bytes` of the suite on this core, ending early where a test ends, since the
    /// next may split. With `vector` the wide tiers' work runs inside [`VectorState::wide`], at most
    /// [`WIDE_CHUNK_BYTES`] at a time.
    pub fn step(&mut self, chunk_bytes: usize, now: Instant, vector: Option<&VectorState>) {
        let Some(run) = self.run.as_mut() else { return };
        if self.waiting.is_some_and(|until| now < until) {
            return;
        }
        if self.paused {
            return;
        }
        self.waiting = None;
        self.history.start(now, self.tally.faults);
        note(&mut self.current, &mut self.history, self.tally.faults, run.suite.schedule(), now);
        let cpu = self.cores.as_ref().map_or(0, Cores::bsp);
        let guard = vector.filter(|_| run.suite.kernels().is_wide());
        let piece = if guard.is_some() { WIDE_CHUNK_BYTES } else { chunk_bytes };
        let mut left = chunk_bytes.max(1);
        let (kind, pass) = (run.suite.test().kind(), run.suite.passes());
        while left > 0 && run.suite.test().kind() == kind && run.suite.passes() == pass {
            let budget = left.min(piece);
            left -= budget;
            let test = run.suite.test().kind().name();
            let tally = &mut self.tally;
            let mut sink = |f: Fault| tally.record(f, test, cpu, pass);
            let mut faults = Faults::new(REPORTS_PER_STEP, &mut sink);
            let suite = &mut run.suite;
            let progress = match guard {
                Some(v) => v.wide(|| suite.step(budget, now, &mut faults)),
                None => suite.step(budget, now, &mut faults),
            };
            let seen = faults.seen();
            self.tally.faults = self.tally.faults.saturating_add(seen);
            if let Progress::Waiting { until } = progress {
                self.waiting = Some(until);
                return;
            }
        }
    }

    /// Run the current test and the split tests after it, in one dispatch of the cores of [`Cores`],
    /// calling `duties` with the sweep's status whenever the instant it last returned has come.
    /// The dispatch takes no new test after [`DISPATCH_BUDGET`], so a longer run takes several. A
    /// core that fails is left out and the test it was on runs again without it; with only the BSP
    /// left, the test is left to [`Sweep::step`].
    pub fn run_split(&mut self, vector: Option<&VectorState>, duties: &mut Duties<'_>) -> Flow {
        let Self { run: Some(run), tally, current, cores: Some(cores), history, .. } = self else {
            return Flow::Continue;
        };
        let started = cores.clock.now();
        history.start(started, tally.faults);
        note(current, history, tally.faults, run.suite.schedule(), started);
        *current = None;
        loop {
            match dispatch(run, tally, cores, history, vector, duties) {
                Dispatch::Finished => return Flow::Continue,
                Dispatch::Stopped => return Flow::Stop,
                Dispatch::Lost(lost) => {
                    let kind = run.suite.test().kind();
                    let before = cores.cpus.len();
                    cores.cpus.retain(|c| !lost.contains(c));
                    if cores.cpus.len() == before {
                        log::error!("sweep: {kind} failed on no core in particular; the BSP runs alone from now");
                        cores.cpus.truncate(1);
                    }
                    history.set_cpus(cores.infos());
                    if cores.cpus.len() <= 1 {
                        log::warn!("sweep: {kind} again on the BSP alone");
                        return Flow::Continue;
                    }
                    log::warn!("sweep: {kind} again on {} cpus", cores.cpus.len());
                },
            }
        }
    }

    /// Log each AP's CR4 and XCR0 against what they were before the sweep.
    pub fn check_cores(&self) {
        if let Some(cores) = &self.cores {
            cores.check_restored();
        }
    }

    #[must_use]
    pub fn status(&self) -> SweepStatus {
        let Some(run) = self.run.as_ref() else { return SweepStatus::IDLE };
        let cpus = self.cpus_for(run.suite.test().kind());
        let mode = if cpus > 1 { Mode::Split } else { Mode::Single };
        SweepStatus { paused: self.paused, ..status_of(run.suite.schedule(), &self.tally, mode, cpus) }
    }

    /// The status with the history and the latest faults, for the screen and the status page.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot<'_> {
        Snapshot { status: self.status(), history: &self.history, faults: &self.tally.recent }
    }
}

/// The status of `schedule`'s current test as `schedule` and `tally` have it.
fn status_of(schedule: &Schedule, tally: &Tally, mode: Mode, cpus: usize) -> SweepStatus {
    let k = schedule.kernels();
    let widest =
        [k.fill, k.verify, k.rmw].into_iter().map(ferrite_mem::Kernel::tier).max().unwrap_or(ferrite_mem::Tier::Scalar);
    SweepStatus {
        bytes: schedule.layout().bytes(),
        test: schedule.test().kind().name(),
        position: schedule.position(),
        test_percent: schedule.test().percent(),
        percent: schedule.percent(),
        passes: schedule.passes(),
        tier: widest.into(),
        mode,
        cpus,
        faults: tally.faults,
        first_fault: tally.first,
        analysis: tally.stats.classification(),
        preset: schedule.preset(),
        paused: false,
    }
}

/// Log and record the test in `current` once the schedule's test is a different one, and start
/// timing that.
fn note(current: &mut Option<Current>, history: &mut History, faults: u64, schedule: &Schedule, now: Instant) {
    let (test, pass) = (schedule.test().kind().name(), schedule.passes());
    let start = Current {
        test,
        pass,
        started: now,
        work_bytes: schedule.test().total().saturating_mul(WORD as u64),
        faults_at_start: faults,
    };
    match *current {
        Some(c) if c.test != test || c.pass != pass => {
            let took = now - c.started;
            let bytes = schedule.layout().bytes();
            log::info!("sweep: pass {} {} took {} ms over {bytes} bytes on 1 cpu", c.pass, c.test, took.as_millis());
            let record = TestRecord {
                pass: c.pass,
                test: c.test,
                cpus: 1,
                took,
                work_bytes: c.work_bytes,
                faults: faults - c.faults_at_start,
            };
            history.finish_test(record, pass, now, faults);
            *current = Some(start);
        },
        None => *current = Some(start),
        Some(_) => {},
    }
}

/// How one dispatch of the cores ended.
enum Dispatch {
    /// Every test the dispatch took is finished; the suite is at the next.
    Finished,
    Stopped,
    /// The cores that failed; the test they were on did not end everywhere.
    Lost(Vec<ProcessorNumber>),
}

/// The firmware resets an AP still in its dispatch after this; generous, since a core is only
/// declared lost once the firmware has given up on it.
fn ap_timeout(test: &Test) -> Duration {
    let bytes = test.total().saturating_mul(WORD as u64);
    Duration::from_secs(bytes / AP_FLOOR_RATE + 1) + test.waits() + AP_PATIENCE + GATHER_PATIENCE + AP_SLACK
}

/// Move the faults the seats kept since the last call into `tally`, each with its processor.
fn absorb(tally: &mut Tally, seats: &[Seat], absorbed: &mut [usize], cpus: &[usize], test: &'static str, pass: u64) {
    for ((seat, taken), &cpu) in seats.iter().zip(absorbed.iter_mut()).zip(cpus) {
        for fault in seat.kept_from(*taken) {
            tally.record(fault, test, cpu, pass);
            *taken += 1;
        }
    }
}

/// `base` brought up to date with the seats' progress and faults.
fn live(base: &SweepStatus, seats: &[Seat], tally: &Tally, faults_before: u64, work: (u64, u64, u64)) -> SweepStatus {
    let (pass_done, pass_total, test_total) = work;
    let done: u64 = seats.iter().map(Seat::done).sum();
    SweepStatus {
        test_percent: suite::percent(done, test_total),
        percent: suite::percent(pass_done + done.min(test_total), pass_total),
        faults: faults_before + seats.iter().map(Seat::faults).sum::<u64>(),
        first_fault: tally.first,
        analysis: tally.stats.classification(),
        ..*base
    }
}

/// The test the cores are on, as the status and the log describe it.
struct Round {
    name: &'static str,
    pass: u64,
    base: SweepStatus,
    faults_before: u64,
    /// Work of the pass before the test, of the whole pass, and of the test.
    work: (u64, u64, u64),
    started: Instant,
}

impl Round {
    fn of(schedule: &Schedule, tally: &Tally, cpus: usize, test_total: u64, started: Instant) -> Self {
        let (pass_done, pass_total) = schedule.pass_work();
        Self {
            name: schedule.test().kind().name(),
            pass: schedule.passes(),
            base: status_of(schedule, tally, Mode::Split, cpus),
            faults_before: tally.faults,
            work: (pass_done, pass_total, test_total),
            started,
        }
    }
}

/// One dispatch of the cores of `cores` over the current test and the split tests after it: shares,
/// jobs and AP dispatches, then the BSP's own share between its duties. The cores go from test to
/// test at a gate ([`Crew::begin_round`]) until the run is done or [`DISPATCH_BUDGET`] has passed.
#[expect(clippy::too_many_lines, reason = "one start, one loop and the reckoning, each needing the others' locals")]
fn dispatch(
    run: &mut Run,
    tally: &mut Tally,
    cores: &mut Cores,
    history: &mut History,
    vector: Option<&VectorState>,
    duties: &mut Duties<'_>,
) -> Dispatch {
    let Some(parts) = NonZeroUsize::new(cores.cpus.len()) else { return Dispatch::Lost(Vec::new()) };
    let (regions, schedule) = run.suite.split();
    let (first, pass, config, kernels) =
        (schedule.test().kind(), schedule.passes(), schedule.config(), schedule.kernels());
    let layout = schedule.layout().clone();
    let kinds: Vec<TestKind> = TestKind::sequence(schedule.preset())
        .skip_while(|&k| k != first)
        .take_while(|k| k.mode() == Mode::Split)
        .collect();
    let clock = cores.clock;
    let crew = Crew::default();
    let shares: Vec<(Span, Regions<'_>)> = suite::shares(regions, first, &config, parts).collect();
    let cpus: Vec<ProcessorNumber> = cores.cpus[..shares.len()].to_vec();
    let cpu_ids: Vec<usize> = cpus.iter().map(|c| c.get()).collect();
    let seats: Vec<Seat> = shares.iter().map(|_| Seat::default()).collect();
    let mut totals = vec![0_u64; kinds.len()];
    let mut longest = Duration::ZERO;
    let jobs: Vec<Job<'_, Clock>> = shares
        .into_iter()
        .zip(&seats)
        .map(|((span, share), seat)| {
            let tests: Vec<Test> = kinds.iter().map(|&k| Test::for_share(k, config, pass, &layout, span)).collect();
            for (total, test) in totals.iter_mut().zip(&tests) {
                *total += test.total();
                longest = longest.max(ap_timeout(test));
            }
            Job::new(tests, share, kernels, WIDE_CHUNK_BYTES, clock, &crew, seat)
        })
        .collect();
    let timeout = DISPATCH_BUDGET + longest;
    let dispatched = clock.now();
    let mut round = Round::of(schedule, tally, cpus.len(), totals[0], dispatched);
    let mut absorbed = vec![0; seats.len()];
    let mut lost: Vec<usize> = Vec::new();
    let mut stop_asked = false;
    let mut completed = 0;
    let panics = mp::ap_panics();
    let mut tend = |now: Instant, tally: &mut Tally, history: &History, round: &Round, absorbed: &mut [usize]| {
        absorb(tally, &seats, absorbed, &cpu_ids, round.name, round.pass);
        let status = live(&round.base, &seats, tally, round.faults_before, round.work);
        duties(now, &Snapshot { status, history, faults: &tally.recent })
    };
    let (live_seats, closed, timing) = cores.mp.scope(|scope| {
        let mut started = Vec::new();
        for (i, (job, &cpu)) in jobs.iter().zip(&cpus).enumerate().skip(1) {
            match scope.start(cpu, job, timeout) {
                Ok(()) => started.push(i),
                Err(e) => {
                    log::warn!("sweep: {}: {e}", round.name);
                    lost.push(i);
                },
            }
        }
        let mut next_duty = clock.now();
        let deadline = next_duty + GATHER_PATIENCE;
        let gathered = loop {
            let now = clock.now();
            if !lost.is_empty() {
                break false;
            }
            if crew.arrived() as usize == started.len() {
                break true;
            }
            if started.iter().any(|&i| seats[i].state().ended()) {
                break false;
            }
            if now > deadline {
                lost.extend(started.iter().filter(|&&i| seats[i].state() != SeatState::Ready));
                break false;
            }
            if now >= next_duty {
                let Some(next) = tend(now, tally, history, &round, &mut absorbed) else {
                    stop_asked = true;
                    break false;
                };
                next_duty = next;
            }
            core::hint::spin_loop();
        };
        let mut live = vec![0];
        live.extend(&started);
        let mut own = jobs.first().and_then(Job::claim);
        let mut own_done = own.is_none();
        let gathered_at = clock.now();
        if gathered {
            crew.release();
            round.started = gathered_at;
            crew.begin_round();
            if let Some(worker) = own.as_mut() {
                worker.begin(crew.round());
            }
        } else {
            crew.abandon();
        }
        let (mut closing, mut closed_at, mut last_check) = (false, gathered_at, gathered_at);
        loop {
            let round_done = crew.coordinate(live.iter().map(|&i| &seats[i]));
            if !own_done && let Some(worker) = own.as_mut() {
                own_done = matches!(worker.turn(vector), Turn::Done | Turn::Stopped);
            }
            let now = clock.now();
            if now >= next_duty {
                if let Some(next) = tend(now, tally, history, &round, &mut absorbed) {
                    next_duty = next;
                } else {
                    stop_asked = true;
                    crew.stop();
                }
            }
            if gathered && round_done && !closing && !crew.stopped() {
                absorb(tally, &seats, &mut absorbed, &cpu_ids, round.name, round.pass);
                tally.faults = round.faults_before.saturating_add(seats.iter().map(Seat::faults).sum());
                let ended = clock.now();
                let took = ended - round.started;
                let n = cpus.len();
                log::info!(
                    "sweep: pass {} {} took {} ms over {} bytes on {n} cpus",
                    round.pass,
                    round.name,
                    took.as_millis(),
                    layout.bytes()
                );
                let work_bytes = schedule.test().total().saturating_mul(WORD as u64);
                schedule.finish_test();
                let faults = tally.faults - round.faults_before;
                let record = TestRecord { pass: round.pass, test: round.name, cpus: n, took, work_bytes, faults };
                history.finish_test(record, schedule.passes(), ended, tally.faults);
                completed += 1;
                if completed < kinds.len() && ended - dispatched < DISPATCH_BUDGET {
                    round = Round::of(schedule, tally, n, totals[completed], ended);
                    absorbed.fill(0);
                    seats.iter().for_each(Seat::reset);
                    crew.begin_round();
                    if let Some(worker) = own.as_mut() {
                        worker.begin(crew.round());
                        own_done = false;
                    }
                } else {
                    crew.close();
                    closing = true;
                    closed_at = ended;
                    if let Some(worker) = own.as_ref() {
                        worker.retire();
                    }
                }
            }
            let winding_down = closing || crew.stopped();
            if winding_down || now - last_check >= CHECK_EVERY {
                last_check = now;
                let running = scope.running();
                let finished = started.iter().filter(|&&i| seats[i].state().ended()).count();
                if !crew.stopped() && (mp::ap_panics() != panics || started.len() - running > finished) {
                    log::warn!("sweep: {}: an AP stopped answering; stopping the others", round.name);
                    crew.stop();
                }
                // A reset AP's seat never ends, so the firmware's word decides when no AP is left.
                if winding_down && own_done && running == 0 {
                    break;
                }
            }
            if own_done {
                core::hint::spin_loop();
            }
        }
        (live, closing, (gathered_at - dispatched, closed_at))
    });
    absorb(tally, &seats, &mut absorbed, &cpu_ids, round.name, round.pass);
    tally.faults = round.faults_before.saturating_add(seats.iter().map(Seat::faults).sum());
    for seat in &seats[1..] {
        if let Some(bytes) = seat.stack_bytes()
            && bytes > cores.deepest_stack
        {
            cores.deepest_stack = bytes;
            if bytes >= STACK_PROBE {
                log::warn!("sweep: an AP used all {STACK_PROBE} probed stack bytes in the run from {}", first.name());
            } else {
                log::debug!(
                    "sweep: deepest AP stack {bytes} of {STACK_PROBE} probed bytes, in the run from {}",
                    first.name()
                );
            }
        }
    }
    if stop_asked {
        return Dispatch::Stopped;
    }
    lost.extend(
        live_seats.iter().skip(1).filter(|&&i| !matches!(seats[i].state(), SeatState::Done | SeatState::Stopped)),
    );
    lost.sort_unstable();
    lost.dedup();
    for &i in &lost {
        log::error!("sweep: {}: {} {}", round.name, cpus[i], seats[i].state());
    }
    if lost.is_empty()
        && live_seats.len() == seats.len()
        && closed
        && seats.iter().all(|s| s.state() == SeatState::Done)
    {
        let (start, closed_at) = timing;
        log::debug!(
            "sweep: {completed} tests in one dispatch on {} cpus: {} ms to start, {} ms from the last test to the end",
            cpus.len(),
            start.as_millis(),
            (clock.now() - closed_at).as_millis()
        );
        Dispatch::Finished
    } else {
        Dispatch::Lost(lost.iter().map(|&i| cpus[i]).collect())
    }
}

/// The largest buffer `try_alloc` gives, starting at `want` bytes and halving down to `min`;
/// `None` when even `min` fails.
fn alloc_largest<T>(want: usize, min: usize, mut try_alloc: impl FnMut(usize) -> Option<T>) -> Option<T> {
    let mut bytes = want;
    loop {
        if let Some(buf) = try_alloc(bytes) {
            if bytes < want {
                log::warn!("sweep: {want} byte buffer unavailable, testing {bytes} bytes");
            }
            return Some(buf);
        }
        if bytes <= min {
            log::error!("sweep: no buffer of {min} bytes or more available, sweep disabled");
            return None;
        }
        bytes = (bytes / 2).max(min);
    }
}

#[cfg(test)]
mod tests;
