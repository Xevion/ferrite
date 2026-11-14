//! The reserve rotation: testing the memory the sweep leaves free for the firmware.
//!
//! The sweep claims every free range but a reserve, so the firmware keeps room for its own
//! allocations (the network buffers, the heap, the Wi-Fi arena). That reserve would never be tested.
//! After each pass the sweep therefore lends the firmware the reserve's size in pages it has just
//! tested, claims the reserve's free pages, runs one pass over them, releases them and takes back
//! what it lent. While the rotation runs the firmware has the lent block to allocate from, which is
//! the lowest of the sweep's own memory so that it serves the same callers (devices that DMA below
//! 4 GiB) as the reserve did. It is checked when the window opens, and how much stayed free is logged
//! when it closes.
//!
//! Pages the firmware holds stay untested: its own image, pool and boot-services data and the Wi-Fi
//! arena, wherever they sit in the reserve's span, as does memory below 1 MiB, free pieces under 1 MiB
//! and any page the firmware took out of the lent block during the window.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::fmt;

use ferrite_efi::memory::ranges::{self, Range};
use ferrite_efi::memory::{self, Claim};
use ferrite_hal::Instant;
use ferrite_mem::suite::{Config, Kernels, Preset};
use uefi::mem::memory_map::MemoryType;

use super::{Coverage, Run, Sweep, note};
use crate::report::History;

/// What opening a window got.
pub struct Opened {
    /// The reserve's free pages, to be tested.
    pub reserve: Claim,
    /// Where the lent block was, in the sweep's own memory.
    pub lent: Vec<Range>,
    /// Bytes the firmware has free once the reserve is claimed.
    pub free: u64,
}

/// What closing a window found.
pub struct Closed {
    /// Bytes of the lent block the firmware kept.
    pub kept: u64,
    /// Bytes the firmware had free just before the reserve was released.
    pub free: u64,
}

/// The firmware's side of the rotation.
pub trait Pages {
    /// Lend the firmware `lend` bytes of `main`'s lowest pages and claim every other free page, to
    /// test. `None`, with `main` as it was, when that would leave the firmware short.
    fn open(&mut self, main: &mut Claim, lend: u64) -> Option<Opened>;

    /// Release `reserve` and take back into `main` the pages of `lent` the firmware has left free.
    fn close(&mut self, main: &mut Claim, reserve: Claim, lent: &[Range]) -> Closed;
}

/// No rotation: for a sweep over memory that did not come from the memory map.
struct NoPages;

impl Pages for NoPages {
    fn open(&mut self, _main: &mut Claim, _lend: u64) -> Option<Opened> {
        None
    }

    fn close(&mut self, _main: &mut Claim, _reserve: Claim, _lent: &[Range]) -> Closed {
        Closed { kept: 0, free: 0 }
    }
}

/// How much the firmware had free around a window.
#[derive(Clone, Copy)]
struct Free {
    start: u64,
    lowest: u64,
}

/// A rotation under way: the sweep's own memory and settings while the reserve is under test.
struct Window {
    main: Claim,
    lent: Vec<Range>,
    /// The pass the reserve is tested as: the one that has just ended.
    pass: u64,
    kernels: Kernels,
    config: Config,
    preset: Preset,
    /// Where the reserve's tests record, so the sweep's own history keeps its passes.
    history: History,
    opened: Instant,
    free: Free,
}

pub(super) struct Rotation {
    pages: Box<dyn Pages>,
    /// Bytes to lend per window; 0 for no rotation.
    lend: u64,
    pub(super) reported_free: u64,
    main_bytes: u64,
    reserve_tested: u64,
    /// The passes completed when the last window opened, so each pass gets one.
    rotated_at: Option<u64>,
    window: Option<Window>,
}

impl Rotation {
    pub(super) fn new(buffer: Option<&Claim>) -> Self {
        Self {
            pages: Box::new(NoPages),
            lend: 0,
            reported_free: buffer.map_or(0, Claim::reported_free),
            main_bytes: buffer.map_or(0, |b| b.bytes() as u64),
            reserve_tested: 0,
            rotated_at: None,
            window: None,
        }
    }

    pub(super) const fn restart(&mut self) {
        self.rotated_at = None;
    }

    /// Where the tests record while a window is open.
    pub(super) fn history(&mut self) -> Option<&mut History> {
        self.window.as_mut().map(|w| &mut w.history)
    }

    /// The sweep's passes completed, while the suite under test is the reserve's.
    pub(super) fn passes_done(&self) -> Option<u64> {
        self.window.as_ref().map(|w| w.pass + 1)
    }

    pub(super) fn coverage(&self) -> Coverage {
        let tested = self.main_bytes + self.reserve_tested;
        Coverage { tested, free: self.reported_free.max(tested) }
    }

    pub(super) const fn is_open(&self) -> bool {
        self.window.is_some()
    }
}

impl Sweep {
    /// Rotate the reserve in after each pass with `pages`, lending `lend` bytes.
    pub(super) fn with_pages(mut self, pages: impl Pages + 'static, lend: u64) -> Self {
        if lend > 0 {
            self.rotation.pages = Box::new(pages);
            self.rotation.lend = lend;
        }
        self
    }

    /// Close the window if its pass is over, and open one if a pass has just ended.
    pub(super) fn rotate(&mut self, now: Instant) {
        if self.window_finished() {
            self.close_window(Some(now));
        }
        if self.rotation_due() {
            self.open_window(now);
        }
    }

    fn window_finished(&self) -> bool {
        match (&self.rotation.window, &self.run) {
            (Some(w), Some(run)) => run.suite.passes() > w.pass,
            _ => false,
        }
    }

    /// A pass has ended, nothing of the next has run and its window has not been had.
    fn rotation_due(&self) -> bool {
        let Some(run) = &self.run else { return false };
        let passes = run.suite.passes();
        let test = run.suite.test();
        self.rotation.lend > 0
            && !self.rotation.is_open()
            && !self.paused
            && passes > 0
            && self.rotation.rotated_at != Some(passes)
            && run.suite.position().0 == 1
            && test.phase() == 0
            && test.done() == 0
    }

    fn open_window(&mut self, now: Instant) {
        let Self { run, rotation, history, tally, current, waiting, .. } = self;
        let Some(Run { suite, _buffer: mut main }) = run.take() else { return };
        let (kernels, config, preset, passes) = (suite.kernels(), suite.config(), suite.preset(), suite.passes());
        // The pass's last test is recorded in the sweep's history before the reserve's tests start.
        history.start(now, tally.faults);
        note(current, history, tally.faults, suite.schedule(), now);
        drop(suite);
        *current = None;
        *waiting = None;
        rotation.rotated_at = Some(passes);
        let Some(Opened { reserve, lent, free }) = rotation.pages.open(&mut main, rotation.lend) else {
            *run = Run::new(main, kernels, config, preset, passes);
            return;
        };
        let lent_bytes: u64 = lent.iter().map(|r| r.bytes()).sum();
        let pass = passes - 1;
        let reserve_bytes = reserve.bytes() as u64;
        if let Some(reserve_run) = Run::new(reserve, kernels, config, preset, pass) {
            log::info!(
                "sweep: reserve rotation after pass {pass}: {} MiB of the reserve under test, {} MiB lent to the firmware, which has {} MiB free",
                reserve_bytes >> 20,
                lent_bytes >> 20,
                free >> 20
            );
            *run = Some(reserve_run);
            rotation.window = Some(Window {
                main,
                lent,
                pass,
                kernels,
                config,
                preset,
                history: history.clone(),
                opened: now,
                free: Free { start: free, lowest: free },
            });
        } else {
            rotation.pages.close(&mut main, Claim::new(Vec::new()), &lent);
            rotation.main_bytes = main.bytes() as u64;
            *run = Run::new(main, kernels, config, preset, passes);
        }
    }

    /// End the window, finished or not, and carry on over the sweep's own memory. `now` is `None`
    /// when the sweep is being reset, which has no use for the history's clock.
    pub(super) fn close_window(&mut self, now: Option<Instant>) {
        let Self { run, rotation, history, current, waiting, cores, .. } = self;
        let Some(Window { mut main, lent, pass, kernels, config, preset, opened, free, .. }) = rotation.window.take()
        else {
            return;
        };
        let finished = run.as_ref().is_some_and(|r| r.suite.passes() > pass);
        let reserve = match run.take() {
            Some(Run { suite, _buffer: reserve }) => {
                drop(suite);
                reserve
            },
            None => Claim::new(Vec::new()),
        };
        let reserve_bytes = reserve.bytes() as u64;
        let closed = rotation.pages.close(&mut main, reserve, &lent);
        if finished {
            rotation.reserve_tested = reserve_bytes;
        }
        rotation.main_bytes = main.bytes() as u64;
        let lowest = free.lowest.min(closed.free);
        let coverage = rotation.coverage();
        let state = if finished { "ended" } else { "cut short" };
        let log_level = if closed.kept > 0 { log::Level::Warn } else { log::Level::Info };
        log::log!(
            log_level,
            "sweep: reserve rotation {state}: the firmware had {} MiB free at the start, {} MiB at the end, lowest {} MiB; {} MiB of the lent block stayed with it; tested {coverage}",
            free.start >> 20,
            closed.free >> 20,
            lowest >> 20,
            closed.kept >> 20
        );
        *run = Run::new(main, kernels, config, preset, pass + 1);
        *current = None;
        *waiting = None;
        history.set_cpus(cores.as_ref().map_or_default(super::cpu_infos));
        if let (Some(now), Some(started)) = (now, history.pass_started) {
            history.pass_started = Some(started + (now - opened));
        }
    }
}

/// The firmware's pages, through the memory map and boot services.
pub struct FirmwarePages;

/// Bytes of each memory type, for the log.
struct Held<'a>(&'a [(MemoryType, u64)]);

impl fmt::Display for Held<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return f.write_str("nothing");
        }
        for (i, (ty, bytes)) in self.0.iter().enumerate() {
            let sep = if i == 0 { "" } else { ", " };
            write!(f, "{sep}{ty:?} {} MiB", bytes >> 20)?;
        }
        Ok(())
    }
}

impl Pages for FirmwarePages {
    fn open(&mut self, main: &mut Claim, bytes: u64) -> Option<Opened> {
        let zone_end = main.blocks().first()?.base().addr().get() as u64;
        let lent = main.lend_lowest(bytes);
        let lent_bytes: u64 = lent.iter().map(|r| r.bytes()).sum();
        if lent.is_empty() {
            return None;
        }
        let Some(map) = memory::free_map(zone_end) else {
            reclaim(main, &lent);
            return None;
        };
        let wanted = ranges::select(&ranges::subtract(map.ranges(), &lent), 0);
        let reserve = memory::claim_ranges(&wanted);
        let free = memory::free_map(0).map_or(0, |m| m.conventional());
        if reserve.blocks().is_empty() || free < lent_bytes / 2 {
            log::warn!(
                "sweep: reserve rotation skipped: {} MiB of the reserve free to test, {} MiB left to the firmware of the {} MiB lent",
                reserve.bytes() >> 20,
                free >> 20,
                lent_bytes >> 20
            );
            drop(reserve);
            reclaim(main, &lent);
            return None;
        }
        log::info!(
            "sweep: reserve rotation: {} MiB claimed in {} ranges out of the {} MiB below the sweep's lowest page; untested, held by the firmware there: {}",
            reserve.bytes() >> 20,
            reserve.blocks().len(),
            zone_end.saturating_sub(ranges::LOW_MEMORY) >> 20,
            Held(map.held())
        );
        Some(Opened { reserve, lent, free })
    }

    fn close(&mut self, main: &mut Claim, reserve: Claim, lent: &[Range]) -> Closed {
        let free = memory::free_map(0).map_or(0, |m| m.conventional());
        drop(reserve);
        Closed { kept: reclaim(main, lent), free }
    }
}

/// Take back into `main` the pages of `lent` that are still free; the bytes the firmware kept.
fn reclaim(main: &mut Claim, lent: &[Range]) -> u64 {
    let lent_bytes: u64 = lent.iter().map(|r| r.bytes()).sum();
    let back = memory::free_map(0).map_or_default(|map| ranges::select(&ranges::intersect(lent, map.all_ranges()), 0));
    let taken = memory::claim_ranges(&back);
    let got = taken.bytes() as u64;
    main.absorb(taken);
    lent_bytes.saturating_sub(got)
}
