//! The memory sweep: the `ferrite-mem` suite run over one buffer, pass after pass.
//!
//! [`Sweep::step`] runs a bounded chunk of the current test so the caller can interleave other
//! work. The buffer is whole pages from boot services, freed on drop. Steps of the AVX tiers run
//! inside [`VectorState::wide`], in chunks of at most [`WIDE_CHUNK_BYTES`], since the firmware's
//! interrupt entry does not save the upper halves of the vector registers.

use core::fmt;
use core::ptr::NonNull;

use ferrite_efi::cpu::VectorState;
use ferrite_hal::Instant;
use ferrite_mem::analysis::{Classification, FaultStats};
use ferrite_mem::suite::{Config, Kernels, Preset, Progress, Suite};
use ferrite_mem::{Caps, Fault, Faults, Region, WIDE_CHUNK_BYTES};
use uefi::boot::{self, AllocateType, MemoryType};

const PAGE: usize = 4096;
const WORD: usize = size_of::<u64>();
/// Faults handed to the statistics per step; every one is still counted.
const REPORTS_PER_STEP: u64 = 256;

/// Smallest buffer worth sweeping when the requested size cannot be allocated.
pub const MIN_SWEEP_BYTES: usize = 1 << 20;

/// Sweep progress for the UI, the status page and the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SweepStatus {
    pub bytes: usize,
    pub test: &'static str,
    /// The test's 1-based position in the pass, and the number of tests in it.
    pub position: (usize, usize),
    /// Progress through the current test and through the pass, 0 to 100.
    pub test_percent: u16,
    pub percent: u16,
    pub passes: u64,
    /// The widest kernel tier in use, for display.
    pub tier: &'static str,
    /// Mismatched words over all passes.
    pub faults: u64,
    pub first_fault: Option<Fault>,
    /// The test that found the first fault.
    pub first_fault_test: &'static str,
    pub analysis: Classification,
}

impl fmt::Display for SweepStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (i, n) = self.position;
        write!(
            f,
            "{} ({i}/{n}) {}% pass={} {}% {} faults={}",
            self.test, self.test_percent, self.passes, self.percent, self.tier, self.faults
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
        faults: 0,
        first_fault: None,
        first_fault_test: "",
        analysis: Classification::Clean,
    };
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
        _backing: alloc::vec::Vec<u64>,
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

#[derive(Default)]
struct Tally {
    faults: u64,
    first: Option<(Fault, &'static str)>,
    stats: FaultStats,
}

pub struct Sweep {
    run: Option<Run>,
    tally: Tally,
    /// The suite has nothing to do before this.
    waiting: Option<Instant>,
    /// The test seen at the last step, and when it was first seen.
    current: Option<(&'static str, u64, Instant)>,
}

impl Sweep {
    /// Allocate a test buffer of up to `bytes`, halving down to [`MIN_SWEEP_BYTES`] when the pool
    /// cannot supply it, and run the standard preset over it with the kernels `caps` allows. With
    /// no buffer the sweep has no work.
    #[must_use]
    pub fn new(bytes: usize, caps: Caps) -> Self {
        let buffer = alloc_largest(bytes, MIN_SWEEP_BYTES, Buffer::pages);
        Self::with_buffer(buffer, Kernels::for_caps(caps), Config::DEFAULT, Preset::Standard)
    }

    fn with_buffer(buffer: Option<Buffer>, kernels: Kernels, config: Config, preset: Preset) -> Self {
        let run = buffer.and_then(|buffer| {
            // SAFETY: the buffer is ours alone and outlives the suite (field order in `Run`).
            match unsafe { Region::from_raw(buffer.base.as_ptr(), buffer.words) } {
                Ok(region) => {
                    log::info!("sweep: {} bytes at {}, kernels {kernels}", buffer.bytes(), region.base());
                    Some(Run { suite: Suite::new(region, kernels, config, preset), _buffer: buffer })
                },
                Err(e) => {
                    log::error!("sweep: unusable buffer: {e}");
                    None
                },
            }
        });
        Self { run, tally: Tally::default(), waiting: None, current: None }
    }

    /// Whether a step now would do work.
    pub fn has_work(&self, now: Instant) -> bool {
        self.run.is_some() && self.waiting.is_none_or(|until| now >= until)
    }

    /// When the suite next has work, if it is waiting.
    pub const fn waiting_until(&self) -> Option<Instant> {
        self.waiting
    }

    /// Run up to `chunk_bytes` of the suite. With `vector` the wide tiers' work runs inside
    /// [`VectorState::wide`], at most [`WIDE_CHUNK_BYTES`] at a time.
    pub fn step(&mut self, chunk_bytes: usize, now: Instant, vector: Option<&VectorState>) {
        let Some(run) = self.run.as_mut() else { return };
        if self.waiting.is_some_and(|until| now < until) {
            return;
        }
        self.waiting = None;
        let (test, passes) = (run.suite.test().kind().name(), run.suite.passes());
        match self.current {
            Some((name, pass, started)) if name != test || pass != passes => {
                let bytes = run.suite.region().bytes();
                log::info!("sweep: pass {pass} {name} took {} ms over {bytes} bytes", (now - started).as_millis());
                self.current = Some((test, passes, now));
            },
            None => self.current = Some((test, passes, now)),
            Some(_) => {},
        }
        let guard = vector.filter(|_| run.suite.kernels().is_wide());
        let piece = if guard.is_some() { WIDE_CHUNK_BYTES } else { chunk_bytes };
        let mut left = chunk_bytes.max(1);
        while left > 0 {
            let budget = left.min(piece);
            left -= budget;
            let test = run.suite.test().kind().name();
            let tally = &mut self.tally;
            let mut sink = |f: Fault| {
                tally.stats.record(&f);
                tally.first.get_or_insert((f, test));
            };
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

    #[must_use]
    pub fn status(&self) -> SweepStatus {
        let Some(run) = self.run.as_ref() else { return SweepStatus::IDLE };
        let suite = &run.suite;
        let k = suite.kernels();
        let widest = [k.fill, k.verify, k.rmw]
            .into_iter()
            .map(ferrite_mem::Kernel::tier)
            .max()
            .unwrap_or(ferrite_mem::Tier::Scalar);
        SweepStatus {
            bytes: suite.region().bytes(),
            test: suite.test().kind().name(),
            position: suite.position(),
            test_percent: suite.test().percent(),
            percent: suite.percent(),
            passes: suite.passes(),
            tier: widest.into(),
            faults: self.tally.faults,
            first_fault: self.tally.first.map(|(f, _)| f),
            first_fault_test: self.tally.first.map_or("", |(_, t)| t),
            analysis: self.tally.stats.classification(),
        }
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
