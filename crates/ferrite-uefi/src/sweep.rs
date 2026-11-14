//! The memory sweep: the `ferrite-mem` suite run over every free range of memory, pass after pass.
//!
//! [`Sweep::step`] runs a bounded chunk of the current test on the BSP so the caller can interleave
//! other work. Tests that split ([`Mode::Split`]) run instead through [`Sweep::run_split`] on every
//! core of [`Cores`] at once, each on its own share, while the BSP keeps its duties through a
//! callback; consecutive split tests share one dispatch of the APs. The memory is whole pages from
//! boot services, one block per free range of the firmware's memory map less a reserve (see
//! [`ferrite_efi::memory`]), freed on drop; after each pass the reserve itself is tested in a
//! rotation (see [`rotation`]). Steps of the AVX tiers run inside [`VectorState::wide`], in chunks
//! of at most [`WIDE_CHUNK_BYTES`], since the firmware's interrupt entry does not save the upper
//! halves of the vector registers.

use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;
use core::num::NonZeroUsize;
use core::time::Duration;

use self::rotation::{FirmwarePages, Rotation};
use ferrite_efi::clock::Clock;
use ferrite_efi::cores::{CoreChoice, Cores};
use ferrite_efi::cpu::VectorState;
use ferrite_efi::memory::{self, Claim};
use ferrite_efi::mp::{self, ProcessorNumber, STACK_PROBE};
use ferrite_hal::{Clock as _, Instant};
use ferrite_mem::analysis::{Classification, FaultStats};
use ferrite_mem::crew::{AP_PATIENCE, Crew, Job, Seat, SeatState, Turn, WideGuard};
use ferrite_mem::suite::{self, Config, Kernels, Mode, Preset, Progress, Schedule, Span, Suite, Test, TestKind};
use ferrite_mem::{Caps, Fault, Faults, Region, Regions, WIDE_CHUNK_BYTES};

use crate::report::{self, CpuInfo, FAULT_LOG, FaultRecord, History, Snapshot, TestRecord};

mod rotation;

const WORD: usize = size_of::<u64>();
/// Faults handed to the statistics per step; every one is still counted.
const REPORTS_PER_STEP: u64 = 256;
/// The slowest per-core rate an AP's dispatch timeout allows for, in bytes per second.
const AP_FLOOR_RATE: u64 = 250_000_000;
/// How long the BSP waits for every AP of a split run to reach the start. An AP that has not
/// begun its job by then is called off rather than waited for until the firmware's timeout.
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
/// What the sweep leaves free at least. The firmware allocates after the sweep has taken its share:
/// the network stack's buffers and the heap, the dashboard, the status page, the fault log and the
/// Wi-Fi shim's arena for a restart, each from whatever is left.
const RESERVE_FLOOR: u64 = 256 << 20;
/// The reserve also grows with the machine, by this share of the free memory: the firmware's own
/// bookkeeping (page tables, the memory map, per-device buffers) scales with installed memory.
const RESERVE_SHARE: u64 = 32;

/// How much of the memory the firmware reports free the sweep has tested.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Coverage {
    /// Bytes tested: every block the sweep holds each pass, and the reserve of the last rotation.
    pub tested: u64,
    /// Bytes the memory map reported free when the sweep took its memory; at least `tested`.
    pub free: u64,
}

impl Coverage {
    pub const NONE: Self = Self { tested: 0, free: 0 };

    /// Tested bytes as a percentage of the free ones, 0 to 100.
    #[must_use]
    pub const fn percent(self) -> u64 {
        memory::percent(self.tested, self.free)
    }
}

impl fmt::Display for Coverage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} of {} MiB free ({}%)", self.tested >> 20, self.free >> 20, self.percent())
    }
}

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
    /// The share of the free memory tested; what the firmware holds stays out of it.
    pub coverage: Coverage,
}

impl fmt::Display for SweepStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (i, n) = self.position;
        let mode: &str = self.mode.into();
        write!(
            f,
            "{} ({i}/{n}) {}% pass={} {}% {} {mode} on {} cpus faults={} tested {}",
            self.test, self.test_percent, self.passes, self.percent, self.tier, self.cpus, self.faults, self.coverage
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
        coverage: Coverage::NONE,
    };
}

/// The processors split tests run on, as the report describes them.
fn cpu_infos(cores: &Cores) -> Vec<CpuInfo> {
    cores
        .processors()
        .map(|p| CpuInfo {
            number: p.number.get(),
            package: p.location.package,
            core: p.location.core,
            thread: p.location.thread,
            bsp: p.bsp,
        })
        .collect()
}

/// How many bytes of `free` memory the sweep leaves: [`RESERVE_FLOOR`] and a share of it, but never
/// more than half, so a small machine still tests most of what it has.
fn reserve_for(free: u64) -> u64 {
    (RESERVE_FLOOR + free / RESERVE_SHARE).min(free / 2)
}

/// The suite over its buffer. Fields drop in order, so the regions go before the memory under them.
struct Run {
    suite: Suite<'static>,
    _buffer: Claim,
    scope: Scope,
}

/// Which memory a run covers, for the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scope {
    /// The sweep's own memory.
    Main,
    /// The firmware's reserve, tested after the pass of that number.
    Reserve,
}

/// How the log names a test's pass: `pass 3`, or `reserve after pass 3`.
struct PassLabel(Scope, u64);

impl fmt::Display for PassLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Scope::Main => write!(f, "pass {}", self.1),
            Scope::Reserve => write!(f, "reserve after pass {}", self.1),
        }
    }
}

impl Run {
    /// The same run, marked as covering the firmware's reserve.
    const fn into_reserve(mut self) -> Self {
        self.scope = Scope::Reserve;
        self
    }

    /// The suite over `buffer`, starting at pass `pass`; `None`, freeing the buffer, when it cannot
    /// be regions.
    fn new(buffer: Claim, kernels: Kernels, config: Config, preset: Preset, pass: u64) -> Option<Self> {
        let made: Result<Vec<Region<'static>>, _> = buffer
            .blocks()
            .iter()
            // SAFETY: each block is ours alone and outlives the suite (field order in `Run`).
            .map(|b| unsafe { Region::from_raw(b.base().as_ptr(), b.words()) })
            .collect();
        match made {
            Ok(regions) => {
                let regions = Regions::new(regions);
                log::info!(
                    "sweep: {} bytes in {} regions, kernels {kernels}, {preset:?}",
                    regions.bytes(),
                    regions.layout().spans().len()
                );
                Some(Self {
                    suite: Suite::starting_at(regions, kernels, config, preset, pass),
                    _buffer: buffer,
                    scope: Scope::Main,
                })
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
    rotation: Rotation,
}

impl Sweep {
    /// Take every free range of memory but a reserve, and run the standard preset over it with the
    /// kernels `caps` allows, split tests on `cores` when given. Without a usable memory map the
    /// sweep takes one block of up to [`FALLBACK_BYTES`], halving down to [`MIN_SWEEP_BYTES`].
    /// With no memory the sweep has no work.
    #[must_use]
    pub fn new(caps: Caps, cores: Option<Cores>) -> Self {
        let claimed = memory::claim_free_memory(reserve_for);
        let lend = claimed.as_ref().map_or(0, Claim::reserve);
        let buffer = claimed.or_else(|| alloc_largest(FALLBACK_BYTES, MIN_SWEEP_BYTES, Claim::any_pages));
        let preset = Preset::Standard;
        let sweep = Self::with_buffer(buffer, Kernels::for_caps(caps), preset.config(), preset, cores);
        sweep.with_pages(FirmwarePages, lend)
    }

    fn with_buffer(
        buffer: Option<Claim>,
        kernels: Kernels,
        config: Config,
        preset: Preset,
        cores: Option<Cores>,
    ) -> Self {
        let rotation = Rotation::new(buffer.as_ref());
        let run = buffer.and_then(|buffer| Run::new(buffer, kernels, config, preset, 0));
        let mut history = History::default();
        history.set_cpus(cores.as_ref().map_or_default(cpu_infos));
        Self { run, tally: Tally::default(), waiting: None, current: None, cores, history, paused: false, rotation }
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
        self.close_window(None);
        self.rotation.restart();
        if let Some(Run { suite, _buffer: buffer, .. }) = self.run.take() {
            let kernels = suite.kernels();
            self.run = Run::new(buffer, kernels, preset.config(), preset, 0);
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
            self.history.set_cpus(cpu_infos(cores));
        }
    }

    /// When the suite next has work, if it is waiting.
    pub const fn waiting_until(&self) -> Option<Instant> {
        self.waiting
    }

    /// Logical processors `kind` would run on now.
    fn cpus_for(&self, kind: TestKind) -> usize {
        match (kind.mode(), &self.cores) {
            (Mode::Split, Some(cores)) => cores.cpus().len().max(1),
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
        if self.run.is_none() || self.waiting.is_some_and(|until| now < until) || self.paused {
            return;
        }
        self.waiting = None;
        self.rotate(now);
        let Some(run) = self.run.as_mut() else { return };
        let history = history_of(&mut self.rotation, &mut self.history);
        history.start(now, self.tally.faults);
        note(&mut self.current, history, self.tally.faults, run.scope, run.suite.schedule(), now);
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
        if let Some(cores) = &self.cores {
            let now = cores.clock().now();
            self.rotate(now);
        }
        let Self { run: Some(run), tally, current, cores: Some(cores), history, rotation, .. } = self else {
            return Flow::Continue;
        };
        let history = history_of(rotation, history);
        let started = cores.clock().now();
        history.start(started, tally.faults);
        note(current, history, tally.faults, run.scope, run.suite.schedule(), started);
        *current = None;
        loop {
            match dispatch(run, tally, cores, history, vector, duties) {
                Dispatch::Finished => return Flow::Continue,
                Dispatch::Stopped => return Flow::Stop,
                Dispatch::Lost(lost) => {
                    let kind = run.suite.test().kind();
                    let before = cores.cpus().len();
                    cores.drop_cpus(&lost);
                    if cores.cpus().len() == before {
                        log::error!("sweep: {kind} failed on no core in particular; the BSP runs alone from now");
                        cores.keep_bsp_only();
                    }
                    history.set_cpus(cpu_infos(cores));
                    if cores.cpus().len() <= 1 {
                        log::warn!("sweep: {kind} again on the BSP alone");
                        return Flow::Continue;
                    }
                    log::warn!("sweep: {kind} again on {} cpus", cores.cpus().len());
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
        let status = status_of(run.suite.schedule(), &self.tally, mode, cpus);
        SweepStatus {
            paused: self.paused,
            passes: self.rotation.passes_done().unwrap_or(status.passes),
            coverage: self.rotation.coverage(),
            ..status
        }
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
        coverage: Coverage::NONE,
    }
}

/// The history the running tests record into: the sweep's own, or a scratch copy while the reserve
/// is being tested, whose tests and pass are not the sweep's.
fn history_of<'a>(rotation: &'a mut Rotation, own: &'a mut History) -> &'a mut History {
    rotation.history().unwrap_or(own)
}

/// Log and record the test in `current` once the schedule's test is a different one, and start
/// timing that.
fn note(
    current: &mut Option<Current>,
    history: &mut History,
    faults: u64,
    scope: Scope,
    schedule: &Schedule,
    now: Instant,
) {
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
            log::info!(
                "sweep: {} {} took {} ms over {bytes} bytes on 1 cpu",
                PassLabel(scope, c.pass),
                c.test,
                took.as_millis()
            );
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

/// The started APs whose seats show they never began their job, so nothing of theirs is running.
fn never_began(started: &[usize], seats: &[Seat]) -> Vec<usize> {
    started.iter().copied().filter(|&i| seats[i].state() == SeatState::Idle).collect()
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
    let Some(parts) = NonZeroUsize::new(cores.cpus().len()) else { return Dispatch::Lost(Vec::new()) };
    let covers = run.scope;
    let (regions, schedule) = run.suite.split();
    let (first, pass, config, kernels) =
        (schedule.test().kind(), schedule.passes(), schedule.config(), schedule.kernels());
    let layout = schedule.layout().clone();
    let kinds: Vec<TestKind> = TestKind::sequence(schedule.preset())
        .skip_while(|&k| k != first)
        .take_while(|k| k.mode() == Mode::Split)
        .collect();
    let clock = cores.clock();
    let crew = Crew::default();
    let shares: Vec<(Span, Regions<'_>)> = suite::shares(regions, first, &config, parts).collect();
    let cpus: Vec<ProcessorNumber> = cores.cpus()[..shares.len()].to_vec();
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
    let mut called_off: Vec<usize> = Vec::new();
    let mut stop_asked = false;
    let mut completed = 0;
    let panics = mp::ap_panics();
    let wide = vector.map(|v| v as &dyn WideGuard);
    let mut tend = |now: Instant, tally: &mut Tally, history: &History, round: &Round, absorbed: &mut [usize]| {
        absorb(tally, &seats, absorbed, &cpu_ids, round.name, round.pass);
        let status = live(&round.base, &seats, tally, round.faults_before, round.work);
        duties(now, &Snapshot { status, history, faults: &tally.recent })
    };
    let (live_seats, closed, timing) = cores.mp().scope(|scope| {
        let mut started = Vec::new();
        for (i, (job, &cpu)) in jobs.iter().zip(&cpus).enumerate().skip(1) {
            match scope.start_cancellable(cpu, job, timeout) {
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
                for i in never_began(&started, &seats) {
                    if scope.cancel(cpus[i]) {
                        called_off.push(i);
                    }
                }
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
        let mut swept = false;
        loop {
            let round_done = crew.coordinate(live.iter().map(|&i| &seats[i]));
            if !own_done && let Some(worker) = own.as_mut() {
                own_done = matches!(worker.turn(wide), Turn::Done | Turn::Stopped);
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
                    "sweep: {} {} took {} ms over {} bytes on {n} cpus",
                    PassLabel(covers, round.pass),
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
                let finished = started.iter().filter(|&&i| seats[i].state().ended()).count();
                // An AP that is still not in its job a while after the run began to end never will be.
                if winding_down && !swept && now - gathered_at >= GATHER_PATIENCE {
                    swept = true;
                    for i in never_began(&started, &seats) {
                        if scope.cancel(cpus[i]) {
                            called_off.push(i);
                        }
                    }
                }
                let running = scope.running();
                let ended_by_firmware = started.len() - called_off.len() - running;
                if !crew.stopped() && (mp::ap_panics() != panics || ended_by_firmware > finished) {
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
            && cores.note_stack(bytes)
        {
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
        let called = if called_off.contains(&i) { ", called off" } else { "" };
        log::error!("sweep: {}: {} {}{called}", round.name, cpus[i], seats[i].state());
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
