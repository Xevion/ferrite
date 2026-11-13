//! `membench`: throughput of every memory-test building block at every usable tier and store kind.
//!
//! It turns on AVX state, allocates the largest buffer it can (up to 1 GiB), and times each block
//! over the whole buffer with the TSC clock, best of a few passes. Wide tiers run in interrupts-off
//! chunks; a second table compares that guard at several chunk sizes against no guard. The tables go
//! to the log, so to the DbC link under the loader and to `\EFI\ferrite\membench.txt`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;
use core::ptr::NonNull;
use core::time::Duration;

use ferrite_efi::bootlog::{BootLog, Echo};
use ferrite_efi::clock::Clock;
use ferrite_efi::cpu::{FpuControl, VectorState, VectorSupport};
use ferrite_efi::link::Link as LoaderLink;
use ferrite_efi::watchdog::Keepalive;
use ferrite_efi::{logger, session};
use ferrite_hal::{Clock as _, Instant};
use ferrite_mem::{Caps, Fault, Faults, Inversion, Kernel, Order, Prsg, Region, Stores, Tier, WIDE_CHUNK_BYTES};
#[cfg(ferrite_app)]
use ferrite_rt as _;
use log::LevelFilter;
use uefi::boot::{self, AllocateType, MemoryType};
use uefi::prelude::*;

const LOG_NAME: &str = "membench";
const WANT_BYTES: usize = 1 << 30;
const MIN_BYTES: usize = 16 << 20;
const PAGE: usize = 4096;
const REPS: usize = 3;
/// Passes still to run are skipped once this much has elapsed, to stay inside a push's budget.
const BUDGET: Duration = Duration::from_secs(50);
const WATCHDOG: Duration = Duration::from_secs(60);
const WATCHDOG_REARM: Duration = Duration::from_secs(5);
const PATTERN: u64 = 0x5a5a_a5a5_0ff0_f00f;
const SEED: u64 = 0x6d65_6d62_656e_6368;
const GUARD_CHUNKS: [usize; 3] = [64 << 10, 256 << 10, 1 << 20];
const FAULT_BUDGET: u64 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Fill,
    Verify,
    FillAddress,
    VerifyAddress,
    FillPrsg,
    VerifyPrsg,
    Invert,
    Copy,
}

impl Op {
    const ALL: [Self; 8] = [
        Self::Fill,
        Self::Verify,
        Self::FillAddress,
        Self::VerifyAddress,
        Self::FillPrsg,
        Self::VerifyPrsg,
        Self::Invert,
        Self::Copy,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::Fill => "fill",
            Self::Verify => "verify",
            Self::FillAddress => "fill-addr",
            Self::VerifyAddress => "verify-addr",
            Self::FillPrsg => "fill-prsg",
            Self::VerifyPrsg => "verify-prsg",
            Self::Invert => "invert",
            Self::Copy => "copy",
        }
    }

    const fn stores(self) -> bool {
        !matches!(self, Self::Verify | Self::VerifyAddress | Self::VerifyPrsg)
    }

    /// Whether `kernel` gives this op a distinct code path worth timing.
    fn applies(self, kernel: Kernel) -> bool {
        match kernel.tier() {
            Tier::RepString => matches!(self, Self::Fill | Self::Copy),
            _ => self.stores() || kernel.stores() == Stores::Temporal,
        }
    }
}

/// How a pass is cut into chunks, and whether each chunk runs with interrupts off.
#[derive(Clone, Copy)]
struct Walk<'v> {
    chunk_bytes: usize,
    guard: Option<&'v VectorState>,
}

/// One timed pass.
struct Pass {
    took: Duration,
    longest_chunk: Duration,
}

struct Bench<'v> {
    clock: Clock,
    started: Instant,
    caps: Caps,
    vector: Option<&'v VectorState>,
    loader: Option<LoaderLink>,
    watchdog: Option<Keepalive>,
    /// Mismatched words over all passes; any is a real fault, since every verify follows its fill.
    faults: u64,
    stopped: bool,
    skipped: usize,
}

impl<'v> Bench<'v> {
    fn keep_going(&mut self) -> bool {
        let now = self.clock.now();
        if let Some(watchdog) = self.watchdog.as_mut() {
            watchdog.tick(now);
        }
        if let Some(link) = self.loader {
            link.poll();
            self.stopped |= link.stop_requested();
        }
        logger::with(BootLog::poll);
        !self.stopped && now - self.started < BUDGET
    }

    /// Run `op` once over `region` with `kernel`, chunk by chunk.
    fn pass(&mut self, op: Op, kernel: Kernel, region: &mut Region<'_>, walk: Walk<'_>, rep: usize) -> Pass {
        let mut found = 0;
        let mut sink = |f: Fault| {
            found += 1;
            log::warn!("membench: {} {kernel}: {f}", op.name());
        };
        let pass = {
            let mut faults = Faults::new(FAULT_BUDGET, &mut sink);
            let (expect, write) = if rep.is_multiple_of(2) { (PATTERN, !PATTERN) } else { (!PATTERN, PATTERN) };
            let step = Inversion { expect, write, order: Order::Ascending };
            let chunk_words = walk.chunk_bytes / size_of::<u64>();
            let mut prsg = Prsg::new(SEED);
            let mut longest_chunk = Duration::ZERO;
            let clock = self.clock;
            let mut timed = |f: &mut dyn FnMut()| {
                let t = clock.now();
                run(walk.guard, kernel, f);
                longest_chunk = longest_chunk.max(clock.now() - t);
            };
            let start = clock.now();
            if op == Op::Copy {
                let (mut src, mut dst) = region.split_at(region.words() / 2);
                for (s, mut d) in src.chunks(chunk_words).zip(dst.chunks(chunk_words)) {
                    timed(&mut || {
                        if let Err(e) = ferrite_mem::copy(kernel, &s, &mut d) {
                            log::error!("membench: {e}");
                        }
                    });
                }
            } else {
                for mut chunk in region.chunks(chunk_words) {
                    timed(&mut || match op {
                        Op::Fill => ferrite_mem::fill(kernel, &mut chunk, PATTERN),
                        Op::Verify => ferrite_mem::verify(kernel, &chunk, PATTERN, &mut faults),
                        Op::FillAddress => ferrite_mem::fill_address(kernel, &mut chunk, 0),
                        Op::VerifyAddress => ferrite_mem::verify_address(kernel, &chunk, 0, &mut faults),
                        Op::FillPrsg => ferrite_mem::fill_prsg(kernel, &mut chunk, &mut prsg, 0),
                        Op::VerifyPrsg => ferrite_mem::verify_prsg(kernel, &chunk, &mut prsg, 0, &mut faults),
                        Op::Invert => ferrite_mem::invert(kernel, &mut chunk, step, &mut faults),
                        Op::Copy => {},
                    });
                }
            }
            Pass { took: clock.now() - start, longest_chunk }
        };
        self.faults += found;
        pass
    }

    /// The best of `REPS` passes, or `None` when the bench was stopped or ran out of time first.
    fn best(&mut self, op: Op, kernel: Kernel, region: &mut Region<'_>, walk: Walk<'_>) -> Option<Pass> {
        let mut best: Option<Pass> = None;
        for rep in 0..REPS {
            if !self.keep_going() {
                self.skipped += REPS - rep;
                break;
            }
            let pass = self.pass(op, kernel, region, walk, rep);
            if best.as_ref().is_none_or(|b| pass.took < b.took) {
                best = Some(pass);
            }
        }
        // An odd number of inversion passes leaves the complement behind; one more puts PATTERN back.
        if op == Op::Invert && REPS % 2 == 1 {
            self.pass(op, kernel, region, walk, 1);
        }
        best
    }

    fn walk(&self, kernel: Kernel, chunk_bytes: usize) -> Walk<'v> {
        Walk { chunk_bytes, guard: if kernel.tier().is_wide() { self.vector } else { None } }
    }

    /// One row per op, one column per kernel, in GB/s of buffer covered per pass.
    fn main_table(&mut self, region: &mut Region<'_>) {
        let kernels: Vec<Kernel> = self.caps.kernels().collect();
        let mut header = String::from("membench: GB/s       ");
        for k in &kernels {
            let _ = write!(header, "{:>11}", alloc::format!("{k}"));
        }
        log::info!("{header}");
        for op in Op::ALL {
            prepare(op, region);
            let mut row = alloc::format!("membench: {:<12}", op.name());
            for &kernel in &kernels {
                let cell = if op.applies(kernel) {
                    let walk = self.walk(kernel, WIDE_CHUNK_BYTES);
                    self.best(op, kernel, region, walk).map(|p| rate(region.bytes(), p.took))
                } else {
                    None
                };
                match cell {
                    Some(gbs) => {
                        let _ = write!(row, "{gbs:>11.2}");
                    },
                    None => row.push_str("          -"),
                }
            }
            log::info!("{row}");
        }
    }

    /// Fill and verify on the wide kernels: no guard, then the guard at each chunk size, with the
    /// longest interrupts-off chunk.
    fn guard_table(&mut self, region: &mut Region<'_>) {
        let Some(vector) = self.vector else { return };
        let wide: Vec<Kernel> = self.caps.kernels().filter(|k| k.tier().is_wide()).collect();
        if wide.is_empty() {
            return;
        }
        let mut header = alloc::format!("membench: guard {:<16}{:>16}", "", "off/256K");
        for c in GUARD_CHUNKS {
            let _ = write!(header, "{:>20}", alloc::format!("on/{}K (max us)", c >> 10));
        }
        log::info!("{header}");
        for op in [Op::Fill, Op::Verify] {
            prepare(op, region);
            for &kernel in wide.iter().filter(|&&k| op.applies(k)) {
                let mut row = alloc::format!("membench: {:<10} {:<10}", op.name(), alloc::format!("{kernel}"));
                let unguarded = self.best(op, kernel, region, Walk { chunk_bytes: WIDE_CHUNK_BYTES, guard: None });
                match unguarded {
                    Some(p) => {
                        let _ = write!(row, "{:>16.2}", rate(region.bytes(), p.took));
                    },
                    None => row.push_str("               -"),
                }
                for chunk_bytes in GUARD_CHUNKS {
                    match self.best(op, kernel, region, Walk { chunk_bytes, guard: Some(vector) }) {
                        Some(p) => {
                            let cell =
                                alloc::format!("{:.2} ({})", rate(region.bytes(), p.took), p.longest_chunk.as_micros());
                            let _ = write!(row, "{cell:>20}");
                        },
                        None => row.push_str("                   -"),
                    }
                }
                log::info!("{row}");
            }
        }
    }
}

fn run(guard: Option<&VectorState>, kernel: Kernel, f: &mut dyn FnMut()) {
    match guard {
        Some(vector) if kernel.tier().is_wide() => vector.wide(f),
        _ => f(),
    }
}

/// Leave the words the op expects to find: a verify follows its fill, the inversion needs PATTERN.
fn prepare(op: Op, region: &mut Region<'_>) {
    let k = Kernel::SCALAR;
    match op {
        Op::Verify | Op::Invert => ferrite_mem::fill(k, region, PATTERN),
        Op::VerifyAddress => ferrite_mem::fill_address(k, region, 0),
        Op::VerifyPrsg => ferrite_mem::fill_prsg(k, region, &mut Prsg::new(SEED), 0),
        Op::Fill | Op::FillAddress | Op::FillPrsg | Op::Copy => {},
    }
}

#[expect(
    clippy::cast_precision_loss,
    reason = "a rate for display; GB-scale byte counts and ns durations stay well inside f64"
)]
fn rate(bytes: usize, took: Duration) -> f64 {
    bytes as f64 / took.as_nanos().max(1) as f64
}

/// The largest page allocation from `want` bytes, halving down to `min`.
fn allocate(want: usize, min: usize) -> Option<(NonNull<u8>, usize)> {
    let mut bytes = want;
    loop {
        if let Ok(ptr) = boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, bytes / PAGE) {
            return Some((ptr, bytes));
        }
        if bytes <= min {
            return None;
        }
        bytes = (bytes / 2).max(min);
    }
}

#[entry]
fn main() -> Status {
    let fpu = FpuControl::read();
    logger::init(BootLog::open(LOG_NAME, Echo::Console), LevelFilter::Debug);
    if !fpu.sse_ready() {
        log::error!("cpu: the firmware left SSE disabled, stopping: {fpu}");
        logger::with(|log| session::finish(log, Duration::ZERO));
        return Status::UNSUPPORTED;
    }
    log::info!("cpu: {fpu}; {}", VectorSupport::probe());
    if let Err(e) = uefi::helpers::init() {
        log::warn!("boot: uefi helpers init failed: {e}");
    }
    let vector = VectorState::enable()
        .inspect(|v| log::info!("cpu: vector state on, {v}; {}; {}", FpuControl::read(), VectorSupport::probe()))
        .inspect_err(|e| log::warn!("cpu: {e}; running the baseline tiers only"))
        .ok();
    let support = VectorSupport::probe();
    // SAFETY: the probe reads CPUID and XCR0 as they are now, after enabling.
    let caps = unsafe { Caps::new(support.avx2_usable(), support.avx512f_usable()) };
    let status = bench(caps, vector.as_ref());
    if let Some(vector) = vector {
        let (saved, _) = vector.xcr0();
        drop(vector);
        log::info!("cpu: vector state restored to cr4 and xcr0 {saved:#x}; {}", FpuControl::read());
    }
    logger::with(|log| session::finish(log, Duration::ZERO));
    status
}

fn bench(caps: Caps, vector: Option<&VectorState>) -> Status {
    let Some((ptr, bytes)) = allocate(WANT_BYTES, MIN_BYTES) else {
        log::error!("membench: no buffer of {MIN_BYTES} bytes or more available");
        return Status::OUT_OF_RESOURCES;
    };
    if bytes < WANT_BYTES {
        log::warn!("membench: {WANT_BYTES} bytes unavailable, testing {bytes}");
    }
    let clock = Clock::start();
    let loader = LoaderLink::locate();
    let mut bench = Bench {
        clock,
        started: clock.now(),
        caps,
        vector,
        loader,
        watchdog: loader.map(|_| Keepalive::new(WATCHDOG, WATCHDOG_REARM, clock.now())),
        faults: 0,
        stopped: false,
        skipped: 0,
    };
    let kernels: Vec<String> = caps.kernels().map(|k| alloc::format!("{k}")).collect();
    log::info!(
        "membench: {bytes} bytes at {:#x}, best tier {:?}, kernels {}",
        ptr.addr(),
        caps.best(),
        kernels.join(" ")
    );
    // SAFETY: the pages were just allocated for us alone and are freed only after the region's last use.
    let region = unsafe { Region::from_raw(ptr.cast::<u64>().as_ptr(), bytes / size_of::<u64>()) };
    let status = match region {
        Ok(mut region) => {
            bench.main_table(&mut region);
            bench.guard_table(&mut region);
            Status::SUCCESS
        },
        Err(e) => {
            log::error!("membench: {e}");
            Status::ABORTED
        },
    };
    let took = clock.now() - bench.started;
    log::info!(
        "membench: done in {} ms, {} faults, {} passes skipped{}",
        took.as_millis(),
        bench.faults,
        bench.skipped,
        if bench.stopped { " (stopped from the desktop)" } else { "" }
    );
    // SAFETY: allocated above with this page count; the region is gone.
    if let Err(e) = unsafe { boot::free_pages(ptr, bytes / PAGE) } {
        log::warn!("membench: freeing the buffer failed: {e}");
    }
    status
}
