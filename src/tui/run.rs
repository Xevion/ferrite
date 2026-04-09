#![cfg_attr(coverage_nightly, coverage(off))]

use std::sync::atomic::Ordering;
use std::sync::{Arc, mpsc};
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
use super::{Segment, TuiConfig, TuiEvent, TuiMakeWriter, TuiTraceGuard, TuiTraceState};

/// Type alias for the boxed tracing layer used with the reload handle.
pub type BoxedTracingLayer =
    Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync>;

/// Handle returned by tracing init, used to hot-swap the output layer
/// (e.g. from stderr to the TUI channel).
pub type TracingReloadHandle =
    tracing_subscriber::reload::Handle<BoxedTracingLayer, tracing_subscriber::Registry>;

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
    tracing_handle: &TracingReloadHandle,
) -> Result<()> {
    let (tui_tx, tui_rx) = mpsc::sync_channel::<TuiEvent>(256);

    // Hot-swap the tracing layer from stderr to the TUI channel.
    // The TuiTraceState lets us reroute back to stderr after the TUI exits.
    let trace_state = Arc::new(TuiTraceState::new());
    let writer = TuiMakeWriter::new(tui_tx.clone(), Arc::clone(&trace_state));
    tracing_handle
        .modify(|layer| {
            *layer = Box::new(
                tracing_subscriber::fmt::layer()
                    .with_writer(writer)
                    .with_ansi(true),
            );
        })
        .expect("tracing reload failed");

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

    let (done_tx, done_rx) = std::sync::mpsc::sync_channel::<()>(1);
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
            let _ = done_tx.send(());
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

    // TUI exited. Reroute tracing to stderr and drain buffered log events.
    drop(TuiTraceGuard::new(trace_state, tui_rx));

    // Wait for the worker with a bounded timeout (recv_timeout is race-free
    // unlike Condvar::wait_timeout — the message sits in the buffer).
    if done_rx.recv_timeout(Duration::from_secs(5)).is_err() {
        eprintln!("Worker did not exit within 5s, forcing exit");
        shutdown::force_exit(2);
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
