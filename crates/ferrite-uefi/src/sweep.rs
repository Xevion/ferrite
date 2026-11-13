//! The memory sweep: the `ferrite-mem` suite run over one buffer, pass after pass.
//!
//! [`Sweep::step`] runs a bounded chunk of the current test on the BSP so the caller can interleave
//! other work. A test that splits ([`Mode::Split`]) runs instead through [`Sweep::run_split`] on
//! every core of [`Cores`] at once, each on its own share, while the BSP keeps its duties through a
//! callback. The buffer is whole pages from boot services, freed on drop. Steps of the AVX tiers
//! run inside [`VectorState::wide`], in chunks of at most [`WIDE_CHUNK_BYTES`], since the
//! firmware's interrupt entry does not save the upper halves of the vector registers.

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
use ferrite_mem::suite::{self, Config, Kernels, Mode, Preset, Progress, Span, Suite, Test};
use ferrite_mem::{Caps, Fault, Faults, Region, WIDE_CHUNK_BYTES};
use uefi::boot::{self, AllocateType, MemoryType};

use crate::crew::{AP_PATIENCE, Crew, Job, Seat, SeatState, Turn};
use crate::report::{self, CpuInfo, FAULT_LOG, FaultRecord, History, Snapshot, TestRecord};

const PAGE: usize = 4096;
const WORD: usize = size_of::<u64>();
/// Faults handed to the statistics per step; every one is still counted.
const REPORTS_PER_STEP: u64 = 256;
/// The slowest per-core rate an AP's dispatch timeout allows for, in bytes per second.
const AP_FLOOR_RATE: u64 = 250_000_000;
/// How long the BSP waits for every AP of a split test to reach the start.
const GATHER_PATIENCE: Duration = Duration::from_secs(2);
/// Added to every AP's dispatch timeout, for the start and for waiting on slower cores at gates.
const AP_SLACK: Duration = Duration::from_secs(10);
/// How often the BSP asks the firmware which AP dispatches have ended.
const CHECK_EVERY: Duration = Duration::from_millis(2);

/// Smallest buffer worth sweeping when the requested size cannot be allocated.
pub const MIN_SWEEP_BYTES: usize = 1 << 20;

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
    pub first_fault: Option<Fault>,
    /// The test that found the first fault.
    pub first_fault_test: &'static str,
    /// The firmware's number of the processor that found the first fault.
    pub first_fault_cpu: usize,
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
        first_fault_test: "",
        first_fault_cpu: 0,
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

/// Memory the sweep owns: boot-services pages, or a host vector in tests.
struct Buffer {
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

impl Buffer {
    fn pages(bytes: usize) -> Option<Self> {
        let pages = bytes / PAGE;
        let ptr = boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, pages).ok()?;
        Some(Self { base: ptr.cast(), words: pages * PAGE / WORD, owner: Owner::Pages(pages) })
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

impl Drop for Buffer {
    fn drop(&mut self) {
        match self.owner {
            Owner::Pages(pages) => {
                // SAFETY: allocated with this page count, and the region over it is gone (see `Run`).
                if let Err(e) = unsafe { boot::free_pages(self.base.cast(), pages) } {
                    log::warn!("sweep: freeing {pages} pages failed: {e}");
                }
            },
            #[cfg(test)]
            Owner::Host { .. } => {},
        }
    }
}

/// The suite over its buffer. Fields drop in order, so the region goes before the memory under it.
struct Run {
    suite: Suite<'static>,
    _buffer: Buffer,
}

impl Run {
    /// The suite over `buffer`; `None`, freeing the buffer, when it cannot be a region.
    fn new(buffer: Buffer, kernels: Kernels, config: Config, preset: Preset) -> Option<Self> {
        // SAFETY: the buffer is ours alone and outlives the suite (field order in `Run`).
        match unsafe { Region::from_raw(buffer.base.as_ptr(), buffer.words) } {
            Ok(region) => {
                log::info!("sweep: {} bytes at {}, kernels {kernels}, {preset:?}", buffer.bytes(), region.base());
                Some(Self { suite: Suite::new(region, kernels, config, preset), _buffer: buffer })
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
    /// Allocate a test buffer of up to `bytes`, halving down to [`MIN_SWEEP_BYTES`] when the pool
    /// cannot supply it, and run the standard preset over it with the kernels `caps` allows, split
    /// tests on `cores` when given. With no buffer the sweep has no work.
    #[must_use]
    pub fn new(bytes: usize, caps: Caps, cores: Option<Cores>) -> Self {
        let buffer = alloc_largest(bytes, MIN_SWEEP_BYTES, Buffer::pages);
        Self::with_buffer(buffer, Kernels::for_caps(caps), Config::DEFAULT, Preset::Standard, cores)
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
            let (kernels, config) = (suite.kernels(), suite.config());
            self.run = Run::new(buffer, kernels, config, preset);
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
    fn cpus_for(&self, kind: suite::TestKind) -> usize {
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
        note(&mut self.current, &mut self.history, self.tally.faults, &run.suite, now);
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

    /// Run the current test split over every core of [`Cores`], calling `duties` with the sweep's
    /// status whenever the instant it last returned has come. A core that fails is left out and
    /// the test runs again without it; with only the BSP left, the test is left to [`Sweep::step`].
    pub fn run_split(&mut self, vector: Option<&VectorState>, duties: &mut Duties<'_>) -> Flow {
        let Self { run: Some(run), tally, current, cores: Some(cores), history, .. } = self else {
            return Flow::Continue;
        };
        let started = cores.clock.now();
        let (kind, pass, bytes) = (run.suite.test().kind(), run.suite.passes(), run.suite.region().bytes());
        history.start(started, tally.faults);
        note(current, history, tally.faults, &run.suite, started);
        *current = None;
        let faults_at_start = tally.faults;
        loop {
            match attempt(run, tally, cores, history, vector, duties) {
                Attempt::Done => {
                    let now = cores.clock.now();
                    let took = now - started;
                    let n = cores.cpus.len();
                    log::info!("sweep: pass {pass} {kind} took {} ms over {bytes} bytes on {n} cpus", took.as_millis());
                    let work_bytes = run.suite.test().total().saturating_mul(WORD as u64);
                    run.suite.finish_test();
                    let faults = tally.faults - faults_at_start;
                    let record = TestRecord { pass, test: kind.name(), cpus: n, took, work_bytes, faults };
                    history.finish_test(record, run.suite.passes(), now, tally.faults);
                    return Flow::Continue;
                },
                Attempt::Stopped => return Flow::Stop,
                Attempt::Lost(lost) => {
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
        SweepStatus { paused: self.paused, ..status_of(&run.suite, &self.tally, mode, cpus) }
    }

    /// The status with the history and the latest faults, for the screen and the status page.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot<'_> {
        Snapshot { status: self.status(), history: &self.history, faults: &self.tally.recent }
    }
}

/// The status of `suite`'s current test as `suite` and `tally` have it.
fn status_of(suite: &Suite<'_>, tally: &Tally, mode: Mode, cpus: usize) -> SweepStatus {
    let k = suite.kernels();
    let widest =
        [k.fill, k.verify, k.rmw].into_iter().map(ferrite_mem::Kernel::tier).max().unwrap_or(ferrite_mem::Tier::Scalar);
    SweepStatus {
        bytes: suite.region().bytes(),
        test: suite.test().kind().name(),
        position: suite.position(),
        test_percent: suite.test().percent(),
        percent: suite.percent(),
        passes: suite.passes(),
        tier: widest.into(),
        mode,
        cpus,
        faults: tally.faults,
        first_fault: tally.first.map(|f| f.fault),
        first_fault_test: tally.first.map_or("", |f| f.test),
        first_fault_cpu: tally.first.map_or(0, |f| f.cpu),
        analysis: tally.stats.classification(),
        preset: suite.preset(),
        paused: false,
    }
}

/// Log and record the test in `current` once the suite's test is a different one, and start
/// timing that.
fn note(current: &mut Option<Current>, history: &mut History, faults: u64, suite: &Suite<'_>, now: Instant) {
    let (test, pass) = (suite.test().kind().name(), suite.passes());
    let start = Current {
        test,
        pass,
        started: now,
        work_bytes: suite.test().total().saturating_mul(WORD as u64),
        faults_at_start: faults,
    };
    match *current {
        Some(c) if c.test != test || c.pass != pass => {
            let took = now - c.started;
            let bytes = suite.region().bytes();
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

enum Attempt {
    Done,
    Stopped,
    /// The cores that failed; the test did not end everywhere.
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
        first_fault: tally.first.map(|f| f.fault),
        first_fault_test: tally.first.map_or("", |f| f.test),
        first_fault_cpu: tally.first.map_or(0, |f| f.cpu),
        analysis: tally.stats.classification(),
        ..*base
    }
}

/// One try at the current test on every core of `cores`: shares, jobs and AP dispatches, then the
/// BSP's own share between its duties, until every core has ended.
#[expect(clippy::too_many_lines, reason = "one start, one loop and the reckoning, each needing the others' locals")]
fn attempt(
    run: &mut Run,
    tally: &mut Tally,
    cores: &mut Cores,
    history: &History,
    vector: Option<&VectorState>,
    duties: &mut Duties<'_>,
) -> Attempt {
    let Some(parts) = NonZeroUsize::new(cores.cpus.len()) else { return Attempt::Lost(Vec::new()) };
    let suite = &mut run.suite;
    let (kind, pass, config, kernels) = (suite.test().kind(), suite.passes(), suite.config(), suite.kernels());
    let test_name = kind.name();
    let words = suite.region().words();
    let (pass_done, pass_total) = suite.pass_work();
    let base = status_of(suite, tally, Mode::Split, cores.cpus.len());
    let faults_before = tally.faults;
    let clock = cores.clock;
    let crew = Crew::default();
    let shares: Vec<(Span, Region<'_>)> = suite::shares(suite.region_mut(), kind, &config, parts).collect();
    let cpus: Vec<ProcessorNumber> = cores.cpus[..shares.len()].to_vec();
    let cpu_ids: Vec<usize> = cpus.iter().map(|c| c.get()).collect();
    let tests: Vec<Test> = shares.iter().map(|&(span, _)| Test::for_share(kind, config, pass, words, span)).collect();
    let timeouts: Vec<Duration> = tests.iter().map(ap_timeout).collect();
    let test_total = tests.iter().map(Test::total).sum();
    let seats: Vec<Seat> = shares.iter().map(|_| Seat::default()).collect();
    let jobs: Vec<Job<'_, Clock>> = tests
        .into_iter()
        .zip(shares)
        .zip(&seats)
        .map(|((test, (_, region)), seat)| Job::new(test, region, kernels, WIDE_CHUNK_BYTES, clock, &crew, seat))
        .collect();
    let mut absorbed = vec![0; seats.len()];
    let mut lost: Vec<usize> = Vec::new();
    let mut stop_asked = false;
    let panics = mp::ap_panics();
    let mut tend = |now: Instant, tally: &mut Tally| {
        absorb(tally, &seats, &mut absorbed, &cpu_ids, test_name, pass);
        let status = live(&base, &seats, tally, faults_before, (pass_done, pass_total, test_total));
        duties(now, &Snapshot { status, history, faults: &tally.recent })
    };
    let live_seats = cores.mp.scope(|scope| {
        let mut started = Vec::new();
        for (i, (job, (&cpu, &timeout))) in jobs.iter().zip(cpus.iter().zip(&timeouts)).enumerate().skip(1) {
            match scope.start(cpu, job, timeout) {
                Ok(()) => started.push(i),
                Err(e) => {
                    log::warn!("sweep: {test_name}: {e}");
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
                let Some(next) = tend(now, tally) else {
                    stop_asked = true;
                    break false;
                };
                next_duty = next;
            }
            core::hint::spin_loop();
        };
        if gathered {
            crew.release();
        } else {
            crew.abandon();
        }
        let mut live = vec![0];
        live.extend(&started);
        let mut own = jobs.first().and_then(Job::claim);
        let mut own_ended = own.is_none();
        let mut last_check = clock.now();
        loop {
            let all_ended = crew.coordinate(live.iter().map(|&i| &seats[i]));
            if !own_ended && let Some(worker) = own.as_mut() {
                own_ended = matches!(worker.turn(vector), Turn::Done | Turn::Stopped);
            }
            let now = clock.now();
            if now >= next_duty {
                if let Some(next) = tend(now, tally) {
                    next_duty = next;
                } else {
                    stop_asked = true;
                    crew.stop();
                }
            }
            if all_ended || now - last_check >= CHECK_EVERY {
                last_check = now;
                let running = scope.running();
                let finished = started.iter().filter(|&&i| seats[i].state().ended()).count();
                if !crew.stopped() && (mp::ap_panics() != panics || started.len() - running > finished) {
                    log::warn!("sweep: {test_name}: an AP stopped answering; stopping the others");
                    crew.stop();
                }
                // A reset AP's seat never ends, so the firmware's word decides when no AP is left.
                if own_ended && running == 0 {
                    break;
                }
            }
            if own_ended {
                core::hint::spin_loop();
            }
        }
        live
    });
    drop(tend);
    absorb(tally, &seats, &mut absorbed, &cpu_ids, test_name, pass);
    tally.faults = faults_before.saturating_add(seats.iter().map(Seat::faults).sum());
    for seat in &seats[1..] {
        if let Some(bytes) = seat.stack_bytes()
            && bytes > cores.deepest_stack
        {
            cores.deepest_stack = bytes;
            if bytes >= STACK_PROBE {
                log::warn!("sweep: {test_name}: an AP used all {STACK_PROBE} probed stack bytes");
            } else {
                log::debug!("sweep: deepest AP stack {bytes} of {STACK_PROBE} probed bytes, in {test_name}");
            }
        }
    }
    if stop_asked {
        return Attempt::Stopped;
    }
    lost.extend(
        live_seats.iter().skip(1).filter(|&&i| !matches!(seats[i].state(), SeatState::Done | SeatState::Stopped)),
    );
    lost.sort_unstable();
    lost.dedup();
    for &i in &lost {
        log::error!("sweep: {test_name}: {} {}", cpus[i], seats[i].state());
    }
    if lost.is_empty() && live_seats.len() == seats.len() && seats.iter().all(|s| s.state() == SeatState::Done) {
        Attempt::Done
    } else {
        Attempt::Lost(lost.iter().map(|&i| cpus[i]).collect())
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
