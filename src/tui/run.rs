#![cfg_attr(coverage_nightly, coverage(off))]

use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::alloc::CompactionGuard;
use crate::alloc::TestBuffer;
use crate::events::{self, RunEvent};
use crate::pattern::Pattern;
use crate::phys::{MapStats, PagemapResolver, PhysResolver};
use crate::runner;
use crate::shutdown;
use crate::units::{Size, UnitSystem};

use super::bridge::EventBridge;
use super::{Segment, TuiConfig, TuiEvent, TuiMakeWriter};

/// Set up the global tracing subscriber with a layered registry.
///
/// - `json_mode`: whether to emit JSON-formatted trace events on stderr
/// - `tui_writer`: if present, adds a human-readable ANSI layer for the TUI channel
///
/// Layer matrix:
/// | Mode              | TUI layer           | stderr layer          |
/// |-------------------|---------------------|-----------------------|
/// | TUI + JSON        | human ANSI -> TUI    | json -> stderr         |
/// | TUI + no JSON     | human ANSI -> TUI    | (none)                 |
/// | no TUI + JSON     | None                | json -> stderr         |
/// | no TUI + no JSON  | None                | human -> stderr        |
pub fn setup_tracing(json_mode: bool, tui_writer: Option<TuiMakeWriter>) {
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::util::SubscriberInitExt;

    let in_nextest = std::env::var("NEXTEST").is_ok();
    let has_tui = tui_writer.is_some();

    let tui_layer = tui_writer.map(|w| {
        tracing_subscriber::fmt::layer()
            .with_writer(w)
            .with_ansi(true)
    });

    // Under nextest, route all tracing through the test writer so output is
    // captured per-test and only shown on failure. Use try_init to tolerate
    // multiple calls within a single test binary.
    if in_nextest {
        let _ = tracing_subscriber::registry()
            .with(tui_layer)
            .with(tracing_subscriber::fmt::layer().with_test_writer())
            .try_init();
        return;
    }

    let stderr_json = json_mode.then(|| {
        tracing_subscriber::fmt::layer()
            .json()
            .with_writer(std::io::stderr)
    });

    let stderr_human_headless = (!json_mode && !has_tui)
        .then(|| tracing_subscriber::fmt::layer().with_writer(std::io::stderr));

    tracing_subscriber::registry()
        .with(tui_layer)
        .with(stderr_json)
        .with(stderr_human_headless)
        .init();
}

/// Resolved test setup passed into [`run_tui_mode`] from the binary.
pub struct TuiTestSetup {
    pub region: TestBuffer,
    pub resolver: Option<PagemapResolver>,
    pub map_stats: Option<MapStats>,
    /// Keeps the compaction guard alive for the duration of the test.
    pub compaction_guard: Option<CompactionGuard>,
}

/// TUI mode: the default interactive experience.
///
/// # Errors
///
/// Returns an error if terminal initialization, TUI event loop, or any
/// worker thread reports a fatal failure.
///
/// # Panics
///
/// Panics if internal mutexes are poisoned (indicates a prior panic
/// in a worker thread).
#[allow(clippy::too_many_lines)]
pub fn run_tui_mode(
    size: usize,
    passes: usize,
    regions_arg: usize,
    sequential: bool,
    mut setup: TuiTestSetup,
    patterns: Vec<Pattern>,
    json_mode: bool,
) -> Result<()> {
    let (tui_tx, tui_rx) = mpsc::sync_channel::<TuiEvent>(256);

    // Set up tracing: human ANSI layer -> TUI channel, stderr layer based on mode
    let writer = TuiMakeWriter::new(tui_tx.clone());
    setup_tracing(json_mode, Some(writer));

    // Compute region count
    let total_words = setup.region.as_u64_slice().len();
    let min_words_per_region = 1024 * 1024; // 8 MiB minimum per region
    let n_regions = if regions_arg > 0 {
        regions_arg
    } else {
        std::thread::available_parallelism()
            .map(std::num::NonZero::get)
            .unwrap_or(1)
    }
    .min(total_words / min_words_per_region)
    .max(1);

    let chunk_words = total_words / n_regions;
    info!(
        regions = n_regions,
        "testing {} across {} region(s) with {} pattern(s)",
        Size::new(size as f64, UnitSystem::Binary),
        n_regions,
        patterns.len()
    );

    let pattern_names: Vec<String> = patterns
        .iter()
        .map(std::string::ToString::to_string)
        .collect();
    let regions: Vec<Arc<Segment>> = (0..n_regions)
        .map(|i| {
            let region_words = if i == n_regions - 1 {
                total_words - i * chunk_words
            } else {
                chunk_words
            };
            Arc::new(Segment::new(
                format!("region-{i}"),
                region_words * 8,
                pattern_names.clone(),
            ))
        })
        .collect();

    // Create the event bus for the runner
    let (event_tx, event_rx) = events::event_bus();

    // Emit global events
    let _ = event_tx.send(RunEvent::RunStart {
        size,
        passes,
        patterns: patterns.clone(),
        regions: n_regions,
        parallel: !sequential,
    });
    if let Some(ref stats) = setup.map_stats {
        let _ = event_tx.send(RunEvent::MapInfo {
            stats: stats.clone(),
        });
    }

    let worker_regions: Vec<Arc<Segment>> = regions.iter().map(Arc::clone).collect();
    let parallel = !sequential;

    let worker_done = Arc::new((Mutex::new(false), Condvar::new()));
    let worker_done2 = Arc::clone(&worker_done);
    let worker = thread::Builder::new()
        .name("test-driver".into())
        .spawn(move || {
            let buf = setup.region.as_u64_slice_mut();

            thread::scope(|s| {
                let chunks: Vec<&mut [u64]> = buf.chunks_mut(chunk_words).collect();
                for (i, chunk) in chunks.into_iter().enumerate() {
                    let tx = event_tx.clone();
                    let tui_region = Arc::clone(&worker_regions[i]);
                    let resolver_ref = setup
                        .resolver
                        .as_ref()
                        .map(|r| r as &(dyn PhysResolver + Sync));
                    let patterns = &patterns;
                    thread::Builder::new()
                        .name(format!("region-{i}"))
                        .spawn_scoped(s, move || {
                            // Wait while paused before each pattern (checked inside run via on_activity isn't ideal,
                            // but the pause loop was per-pattern-start, so we handle it here at region level)
                            let on_activity = |pos: f64| {
                                tui_region.activity.touch(pos);
                            };

                            let result = runner::run(
                                chunk,
                                i,
                                patterns,
                                passes,
                                parallel,
                                &tx,
                                resolver_ref,
                                &on_activity,
                            );

                            if let Err(e) = result {
                                warn!(region = i, "runner error: {e}");
                            }
                        })
                        .expect("failed to spawn region worker thread");
                }
            });

            // All region threads have finished -- signal completion
            let _ = event_tx.send(RunEvent::RunComplete);

            let (lock, cvar) = &*worker_done2;
            *lock.lock().unwrap() = true;
            cvar.notify_one();
        })
        .expect("failed to spawn test-driver thread");

    // Bridge thread: receives RunEvents, updates Segment state, forwards to TUI channel
    let bridge_regions: Vec<Arc<Segment>> = regions.iter().map(Arc::clone).collect();
    let bridge_tui_tx = tui_tx.clone();
    let bridge_handle = thread::Builder::new()
        .name("event-bridge".into())
        .spawn(move || {
            let bridge = EventBridge::new(bridge_regions, bridge_tui_tx, passes);
            bridge.run(&event_rx);
        })
        .expect("failed to spawn event-bridge thread");

    let config = TuiConfig::default();
    crate::tui::run_tui(&config, &regions, &tui_tx, &tui_rx).context("TUI failed")?;

    // Wait for the worker with a bounded timeout.
    {
        let (lock, cvar) = &*worker_done;
        let guard = lock.lock().unwrap();
        let (done, _) = cvar.wait_timeout(guard, Duration::from_secs(5)).unwrap();
        if !*done {
            eprintln!("Worker did not exit within 5s, forcing exit");
            shutdown::force_exit(2);
        }
    }
    let _ = worker.join();
    if bridge_handle.join().is_err() {
        eprintln!("event bridge thread panicked");
    }

    let total_failures: usize = regions
        .iter()
        .map(|r| r.failure_count.load(Ordering::Relaxed))
        .sum();

    std::process::exit(shutdown::exit_code(total_failures))
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};
    use serial_test::serial;

    use crate::events::{self, RegionEvent, RunEvent};
    use crate::pattern::Pattern;
    use crate::runner;
    use crate::shutdown;

    #[test]
    #[serial]
    fn runner_sends_events_for_tui() {
        shutdown::reset();
        let mut buf = vec![0u64; 1024];
        let (tx, rx) = events::event_bus();

        let results = runner::run(
            &mut buf,
            0,
            &[Pattern::SolidBits],
            1,
            false,
            &tx,
            None,
            &|_| {},
        )
        .unwrap();

        drop(tx);

        check!(results.len() == 1);
        check!(results[0].total_failures() == 0);

        let event_count = rx.try_iter().count();
        assert!(event_count > 0);
    }

    #[test]
    #[serial]
    fn runner_progress_events() {
        shutdown::reset();
        let mut buf = vec![0u64; 1024];
        let (tx, rx) = events::event_bus();

        let _ = runner::run(
            &mut buf,
            0,
            &[Pattern::SolidBits],
            1,
            false,
            &tx,
            None,
            &|_| {},
        )
        .unwrap();

        drop(tx);

        let events: Vec<_> = rx.try_iter().collect();
        let progress_count = events
            .iter()
            .filter(|e| matches!(e, RunEvent::Region(_, RegionEvent::Progress { .. })))
            .count();
        // SolidBits has 2 sub-passes
        check!(progress_count == 2);
    }

    #[test]
    #[serial]
    fn runner_respects_quit_flag() {
        shutdown::reset();
        shutdown::request_quit(shutdown::QuitReason::UserQuit);
        let mut buf = vec![0u64; 1024];
        let (tx, _rx) = events::event_bus();

        let results =
            runner::run(&mut buf, 0, Pattern::ALL, 100, false, &tx, None, &|_| {}).unwrap();

        check!(results.is_empty());
    }

    #[test]
    #[serial]
    fn runner_multi_pass() {
        shutdown::reset();
        let mut buf = vec![0u64; 1024];
        let (tx, _rx) = events::event_bus();

        let results = runner::run(
            &mut buf,
            0,
            &[Pattern::SolidBits],
            3,
            false,
            &tx,
            None,
            &|_| {},
        )
        .unwrap();

        check!(results.len() == 3);
        for r in &results {
            check!(r.total_failures() == 0);
        }
    }

    #[test]
    #[serial]
    fn runner_zero_errors_on_clean_memory() {
        shutdown::reset();
        let mut buf = vec![0u64; 1024];
        let (tx, _rx) = events::event_bus();

        let results = runner::run(&mut buf, 0, Pattern::ALL, 1, false, &tx, None, &|_| {}).unwrap();

        let total: usize = results.iter().map(runner::PassResult::total_failures).sum();
        check!(total == 0);
    }
}
