#![cfg_attr(coverage_nightly, coverage(off))]

use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use snafu::{ResultExt, Whatever};
use tracing::{info, warn};

use crate::alloc::CompactionGuard;
use crate::alloc::TestBuffer;
use crate::events::{self, RunEvent};
use crate::ndjson::NdjsonEventWriter;
use crate::pattern::Pattern;
use crate::phys::{MapStats, PagemapResolver, PhysResolver};
use crate::runner::{self, PassResult, RunConfig, RunResults};
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
    pub buffer: TestBuffer,
    pub resolver: Option<PagemapResolver>,
    pub map_stats: Option<MapStats>,
    /// Keeps the compaction guard alive for the duration of the test.
    pub compaction_guard: Option<CompactionGuard>,
}

/// TUI mode: the default interactive experience.
///
/// # Errors
///
/// Returns an error if terminal initialization, the TUI event loop, or the
/// worker thread reports a fatal failure.
///
/// # Panics
///
/// Panics if internal mutexes are poisoned (indicates a prior panic
/// in the worker thread).
#[expect(
    clippy::too_many_lines,
    reason = "tightly-coupled TUI worker setup/teardown; see clippy.toml too-many-lines-threshold note"
)]
pub fn run_tui_mode(
    size: usize,
    passes: usize,
    workers: usize,
    mut setup: TuiTestSetup,
    patterns: Vec<Pattern>,
    tracing_handle: &TracingReloadHandle,
    events_writer: Option<NdjsonEventWriter>,
) -> Result<RunResults, Whatever> {
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

    info!(
        workers,
        "testing {} with {} worker thread(s), {} pattern(s)",
        Size::new(size as f64, UnitSystem::Binary),
        workers,
        patterns.len()
    );

    let patterns_for_config = patterns.clone();
    let pattern_names: Vec<String> = patterns
        .iter()
        .map(std::string::ToString::to_string)
        .collect();
    let segment = Arc::new(Segment::new(
        Size::new(size as f64, UnitSystem::Binary).to_string(),
        size,
        pattern_names,
    ));

    // Create the event bus for the runner
    let (event_tx, event_rx) = events::event_bus();

    // Emit global events
    let _ = event_tx.send(RunEvent::RunStart {
        size,
        passes,
        patterns: patterns.clone(),
        workers,
    });
    if let Some(ref stats) = setup.map_stats {
        let _ = event_tx.send(RunEvent::MapInfo {
            stats: stats.clone(),
        });
    }

    let parallel = workers > 1;

    // Measure coverage before `setup` is moved into the worker thread.
    let coverage = crate::sysmem::coverage_for(setup.map_stats.as_ref());

    let run_start = std::time::Instant::now();

    // Pass results produced by the worker thread, collected for post-TUI rendering.
    let collected_results: Arc<Mutex<Option<Vec<PassResult>>>> = Arc::new(Mutex::new(None));
    let worker_collected = Arc::clone(&collected_results);
    let worker_segment = Arc::clone(&segment);

    let (done_tx, done_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let worker = thread::Builder::new()
        .name("test-driver".into())
        .spawn(move || {
            let buf = setup.buffer.as_u64_slice_mut();
            let resolver_ref = setup
                .resolver
                .as_ref()
                .map(|r| r as &(dyn PhysResolver + Sync));
            let on_activity = |pos: f64| {
                worker_segment.activity.touch(pos);
            };

            match runner::run(
                buf,
                &patterns,
                passes,
                parallel,
                &event_tx,
                resolver_ref,
                &on_activity,
            ) {
                Ok(pass_results) => {
                    *worker_collected.lock().unwrap() = Some(pass_results);
                }
                Err(e) => {
                    warn!("runner error: {e}");
                }
            }

            let _ = event_tx.send(RunEvent::RunComplete);
            let _ = done_tx.send(());
        })
        .expect("failed to spawn test-driver thread");

    // Bridge thread: receives RunEvents, updates Segment state, forwards to TUI channel,
    // and optionally writes NDJSON events to a file.
    let bridge_segment = Arc::clone(&segment);
    let bridge_tui_tx = tui_tx.clone();
    let bridge_handle = thread::Builder::new()
        .name("event-bridge".into())
        .spawn(move || {
            let bridge = EventBridge::new(bridge_segment, bridge_tui_tx, passes);
            bridge.run(&event_rx, events_writer)
        })
        .expect("failed to spawn event-bridge thread");

    let config = TuiConfig::default();
    crate::tui::run_tui(&config, &segment, &tui_tx, &tui_rx).whatever_context("TUI failed")?;

    // TUI exited. Reroute tracing to stderr and drain buffered log events.
    drop(TuiTraceGuard::new(trace_state, tui_rx));

    // Wait for the worker with a bounded timeout (recv_timeout is race-free
    // unlike Condvar::wait_timeout — the message sits in the buffer).
    if done_rx.recv_timeout(Duration::from_secs(5)).is_err() {
        eprintln!("Worker did not exit within 5s, forcing exit");
        shutdown::force_exit(2);
    }
    let _ = worker.join();
    let mut events_writer = bridge_handle.join().unwrap_or_else(|_| {
        eprintln!("event bridge thread panicked");
        None
    });

    let run_elapsed = run_start.elapsed();

    let pass_results = Arc::try_unwrap(collected_results)
        .expect("worker thread has exited")
        .into_inner()
        .unwrap()
        .unwrap_or_default();

    let config = RunConfig {
        size,
        passes,
        patterns: patterns_for_config,
        workers,
    };
    let mut results = RunResults::from_passes(pass_results, config, run_elapsed);
    results.coverage = coverage;

    // Write the summary run_complete event to the NDJSON file
    if let Some(w) = events_writer.as_mut() {
        w.write_run_complete(
            passes,
            results.total_failures,
            run_elapsed,
            results.coverage,
        );
    }

    Ok(results)
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};
    use serial_test::serial;

    use crate::events::{self, RunEvent};
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
            .filter(|e| matches!(e, RunEvent::Progress { .. }))
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

        let results = runner::run(&mut buf, Pattern::ALL, 100, false, &tx, None, &|_| {}).unwrap();

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

        let results = runner::run(&mut buf, Pattern::ALL, 1, false, &tx, None, &|_| {}).unwrap();

        let total: usize = results.iter().map(runner::PassResult::total_failures).sum();
        check!(total == 0);
    }
}
