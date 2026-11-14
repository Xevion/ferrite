//! What the sweep has done so far, for the screen and the status page: one record per finished
//! test and pass, the most recent faults in full, and the processors split tests run on.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::time::Duration;

use ferrite_hal::Instant;
use ferrite_mem::Fault;
use ferrite_mem::suite::TestKind;

use crate::sweep::SweepStatus;

/// Finished tests kept, newest last; enough for several passes of the thorough preset.
pub const TEST_LOG: usize = 128;
pub const PASS_LOG: usize = 64;
/// Faults kept in full, newest last; every fault is still counted in [`SweepStatus::faults`].
pub const FAULT_LOG: usize = 64;
const TEST_KINDS: usize = TestKind::ALL.len();

/// One test of one pass, finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TestRecord {
    pub pass: u64,
    pub test: &'static str,
    /// Logical processors it ran on.
    pub cpus: usize,
    pub took: Duration,
    /// Bytes the test read or wrote over the whole buffer, counting every phase.
    pub work_bytes: u64,
    /// Mismatched words it found.
    pub faults: u64,
}

impl TestRecord {
    /// Bytes per second over the test, waits included; `None` for a test that took no time.
    #[must_use]
    pub fn rate(&self) -> Option<u64> {
        let nanos = self.took.as_nanos();
        (nanos > 0).then(|| (u128::from(self.work_bytes) * 1_000_000_000 / nanos) as u64)
    }
}

/// One pass over every test of the preset, finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PassRecord {
    pub pass: u64,
    pub took: Duration,
    pub faults: u64,
}

/// A fault kept in full, with where it was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FaultRecord {
    pub fault: Fault,
    pub test: &'static str,
    /// The firmware's number of the processor that found it.
    pub cpu: usize,
    pub pass: u64,
}

/// A logical processor split tests run on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuInfo {
    /// The firmware's processor number.
    pub number: usize,
    pub package: u32,
    pub core: u32,
    pub thread: u32,
    pub bsp: bool,
}

/// Finished tests and passes since the run (re)started. `revision` changes whenever anything here
/// does, so a consumer can skip work on an unchanged history.
#[derive(Clone, Debug, Default)]
pub struct History {
    pub revision: u64,
    /// When the first test of this run started.
    pub run_started: Option<Instant>,
    /// When the current pass started.
    pub pass_started: Option<Instant>,
    pub tests: VecDeque<TestRecord>,
    pub passes: VecDeque<PassRecord>,
    /// The processors split tests use now, the BSP first; empty without MP Services.
    pub cpus: Vec<CpuInfo>,
    /// Faults counted when the current pass started.
    pass_faults_start: u64,
}

impl History {
    /// Note that work started at `now`, if nothing has yet.
    pub fn start(&mut self, now: Instant, faults: u64) {
        if self.run_started.is_none() {
            self.run_started = Some(now);
            self.revision += 1;
        }
        if self.pass_started.is_none() {
            self.pass_started = Some(now);
            self.pass_faults_start = faults;
            self.revision += 1;
        }
    }

    /// Record a finished test; when `next_pass` is past its pass, that pass ended at `now` too.
    pub fn finish_test(&mut self, record: TestRecord, next_pass: u64, now: Instant, faults: u64) {
        push_bounded(&mut self.tests, record, TEST_LOG);
        if next_pass > record.pass {
            let took = self.pass_started.map_or(Duration::ZERO, |s| now - s);
            let pass = PassRecord { pass: record.pass, took, faults: faults - self.pass_faults_start };
            push_bounded(&mut self.passes, pass, PASS_LOG);
            self.pass_started = Some(now);
            self.pass_faults_start = faults;
        }
        self.revision += 1;
    }

    pub fn set_cpus(&mut self, cpus: Vec<CpuInfo>) {
        if self.cpus != cpus {
            self.cpus = cpus;
            self.revision += 1;
        }
    }

    /// Forget every test and pass, keeping the processors, for a restarted run.
    pub fn reset(&mut self) {
        let cpus = core::mem::take(&mut self.cpus);
        let revision = self.revision + 1;
        *self = Self { revision, cpus, ..Self::default() };
    }

    /// The latest record of each test, in the order they last ran.
    pub fn latest_tests(&self) -> impl Iterator<Item = &TestRecord> {
        let mut latest: heapless::Vec<&TestRecord, TEST_KINDS> = heapless::Vec::new();
        for record in self.tests.iter().rev() {
            if !latest.iter().any(|l| l.test == record.test) && latest.push(record).is_err() {
                break;
            }
        }
        latest.reverse();
        latest.into_iter()
    }
}

pub(crate) fn push_bounded<T>(log: &mut VecDeque<T>, item: T, cap: usize) {
    if log.len() == cap {
        log.pop_front();
    }
    log.push_back(item);
}

/// The sweep as the screen and the status page see it at one instant.
#[derive(Clone, Copy, Debug)]
pub struct Snapshot<'a> {
    pub status: SweepStatus,
    pub history: &'a History,
    /// The most recent faults kept in full, newest last.
    pub faults: &'a VecDeque<FaultRecord>,
}
