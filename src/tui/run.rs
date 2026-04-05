#![cfg_attr(coverage_nightly, coverage(off))]

use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::alloc::CompactionGuard;
use crate::alloc::LockedRegion;
use crate::edac::EdacSnapshot;
use crate::output::OutputSink;
use crate::pattern::{Pattern, run_pattern};
use crate::phys::{MapStats, PagemapResolver, PhysResolver};
use crate::shutdown;
use crate::units::{Size, UnitSystem};

use super::{RegionState, TuiConfig, TuiError, TuiEvent, TuiMakeWriter};

/// Set up the global tracing subscriber with a layered registry.
///
/// - `json_mode`: whether to emit JSON-formatted trace events on stderr
/// - `tui_writer`: if present, adds a human-readable ANSI layer for the TUI channel
///
/// Layer matrix:
/// | Mode              | TUI layer           | stderr layer          |
/// |-------------------|---------------------|-----------------------|
/// | TUI + JSON        | human ANSI -> TUI    | json -> stderr         |
/// | TUI + no JSON     | human ANSI -> TUI    | human no-ANSI -> stderr|
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

    let stderr_human_tui = (!json_mode && has_tui).then(|| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(std::io::stderr)
    });

    tracing_subscriber::registry()
        .with(tui_layer)
        .with(stderr_json)
        .with(stderr_human_headless)
        .with(stderr_human_tui)
        .init();
}

/// Resolved test setup passed into [`run_tui_mode`] from the binary.
pub struct TuiTestSetup {
    pub region: LockedRegion,
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
/// Panics if the output sink mutex is poisoned (indicates a prior panic
/// in a worker thread).
pub fn run_tui_mode(
    size: usize,
    passes: usize,
    regions_arg: usize,
    sequential: bool,
    mut setup: TuiTestSetup,
    patterns: Vec<Pattern>,
    sink: OutputSink,
) -> Result<()> {
    let (tx, rx) = mpsc::sync_channel::<TuiEvent>(256);

    let json_mode = sink.is_json();
    let sink = Arc::new(Mutex::new(sink));

    // Set up tracing: human ANSI layer -> TUI channel, stderr layer based on mode
    let writer = TuiMakeWriter::new(tx.clone());
    setup_tracing(json_mode, Some(writer));

    if let Some(ref stats) = setup.map_stats {
        sink.lock().unwrap().emit_map_info(stats);
    }

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
    let regions: Vec<Arc<RegionState>> = (0..n_regions)
        .map(|i| {
            let region_words = if i == n_regions - 1 {
                total_words - i * chunk_words
            } else {
                chunk_words
            };
            Arc::new(RegionState::new(
                format!("region-{i}"),
                region_words * 8,
                pattern_names.clone(),
            ))
        })
        .collect();

    let worker_regions: Vec<Arc<RegionState>> = regions.iter().map(Arc::clone).collect();
    let worker_tx = tx.clone();
    let parallel = !sequential;

    let worker_done = Arc::new((Mutex::new(false), Condvar::new()));
    let worker_done2 = Arc::clone(&worker_done);
    let worker_sink = Arc::clone(&sink);
    let worker = thread::Builder::new()
        .name("test-driver".into())
        .spawn(move || {
            let buf = setup.region.as_u64_slice_mut();

            thread::scope(|s| {
                let chunks: Vec<&mut [u64]> = buf.chunks_mut(chunk_words).collect();
                for (i, chunk) in chunks.into_iter().enumerate() {
                    let tui_region = Arc::clone(&worker_regions[i]);
                    let tx = worker_tx.clone();
                    let resolver_ref = setup
                        .resolver
                        .as_ref()
                        .map(|r| r as &(dyn PhysResolver + Sync));
                    let patterns = &patterns;
                    let sink = &worker_sink;
                    thread::Builder::new()
                        .name(format!("region-{i}"))
                        .spawn_scoped(s, move || {
                            run_region_worker(
                                chunk,
                                patterns,
                                passes,
                                parallel,
                                i,
                                &tui_region,
                                &tx,
                                resolver_ref,
                                sink,
                            );
                        })
                        .expect("failed to spawn region worker thread");
                }
            });

            let (lock, cvar) = &*worker_done2;
            *lock.lock().unwrap() = true;
            cvar.notify_one();
        })
        .expect("failed to spawn test-driver thread");

    let config = TuiConfig::default();
    let run_start = Instant::now();
    crate::tui::run_tui(&config, &regions, &tx, &rx).context("TUI failed")?;

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

    let total_errors: usize = regions
        .iter()
        .map(|r| r.error_count.load(Ordering::Relaxed))
        .sum();

    {
        let elapsed = run_start.elapsed();
        let mut sink = sink.lock().unwrap();
        sink.emit_summary(passes, total_errors, elapsed);
        sink.print_final_result(total_errors);
    }

    std::process::exit(shutdown::exit_code(total_errors))
}

/// Worker for a single memory region: runs test patterns and feeds results to the TUI.
///
/// # Panics
///
/// Panics if the output sink mutex is poisoned.
#[allow(clippy::too_many_arguments)]
pub fn run_region_worker(
    buf: &mut [u64],
    patterns: &[Pattern],
    passes: usize,
    parallel: bool,
    region_idx: usize,
    tui_state: &Arc<RegionState>,
    tx: &mpsc::SyncSender<TuiEvent>,
    resolver: Option<&(dyn PhysResolver + Sync)>,
    sink: &Mutex<OutputSink>,
) {
    let region_bytes = buf.len() * 8;
    info!(
        region = tui_state.name.as_str(),
        "testing {} across {} pass(es) with {} pattern(s)",
        Size::new(region_bytes as f64, UnitSystem::Binary),
        passes,
        patterns.len()
    );

    for pass in 0..passes {
        if shutdown::quit_requested() {
            break;
        }

        let edac_before = EdacSnapshot::capture();

        for (pat_idx, &pattern) in patterns.iter().enumerate() {
            if shutdown::quit_requested() {
                break;
            }

            tui_state.set_pattern(pat_idx);
            sink.lock().unwrap().emit_test_start(pattern, pass);
            info!(region = tui_state.name.as_str(), pattern = %pattern, pass = pass + 1, "starting pattern");

            while tui_state.paused.load(Ordering::Relaxed) && !shutdown::quit_requested() {
                thread::sleep(Duration::from_millis(50));
            }

            let sub_passes = pattern.sub_passes();
            let start = Instant::now();
            let mut sub_pass_count: u64 = 0;

            let on_activity = |pos: f64| {
                tui_state.activity.touch(pos);
            };
            let mut failures = run_pattern(
                pattern,
                buf,
                parallel,
                &mut || {
                    sub_pass_count += 1;
                    let bp = (sub_pass_count * 10000) / sub_passes;
                    tui_state.progress_bp.store(bp, Ordering::Relaxed);
                },
                &on_activity,
            );
            let elapsed = start.elapsed();

            if let Some(resolver) = resolver {
                for f in &mut failures {
                    f.phys_addr = resolver.resolve(f.addr).ok();
                }
            }

            for f in &failures {
                tui_state.record_error();
                let _ = tx.try_send(TuiEvent::Error(TuiError {
                    region_idx,
                    region_name: tui_state.name.clone(),
                    address: f.addr as u64,
                    expected: f.expected,
                    actual: f.actual,
                    bit_position: f.xor().trailing_zeros() as u8,
                    pattern: pattern.to_string(),
                    progress_fraction: sub_pass_count as f64 / sub_passes as f64,
                }));
            }

            tui_state.progress_bp.store(10000, Ordering::Relaxed);
            let bytes_processed = buf.len() as u64 * 8 * 2 * sub_passes;
            sink.lock().unwrap().emit_test_complete(
                pattern,
                pass,
                elapsed,
                bytes_processed,
                &failures,
            );
            info!(
                region = tui_state.name.as_str(),
                pattern = %pattern,
                pass = pass + 1,
                elapsed_ms = elapsed.as_secs_f64() * 1000.0,
                errors = failures.len(),
                "pattern complete"
            );
        }

        // EDAC check
        if let (Some(before), Some(after)) = (&edac_before, EdacSnapshot::capture()) {
            let deltas = before.delta(&after);
            sink.lock().unwrap().emit_ecc_deltas(pass, &deltas);
            for d in &deltas {
                warn!(
                    mc = d.mc,
                    dimm = d.dimm_index,
                    ce = d.ce_delta,
                    ue = d.ue_delta,
                    "ECC event detected"
                );
            }
        }
    }

    let _ = tx.try_send(TuiEvent::RegionDone(region_idx));
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex, mpsc};

    use assert2::{assert, check};
    use serial_test::serial;

    use crate::output::OutputSink;
    use crate::pattern::Pattern;
    use crate::shutdown;
    use crate::units::UnitSystem;

    use super::*;

    fn make_test_state(patterns: &[Pattern]) -> Arc<RegionState> {
        let names: Vec<String> = patterns.iter().map(ToString::to_string).collect();
        Arc::new(RegionState::new("test-region".into(), 8192, names))
    }

    fn make_sink() -> Mutex<OutputSink> {
        Mutex::new(OutputSink::human(UnitSystem::Binary))
    }

    #[test]
    #[serial]
    fn worker_sends_region_done() {
        shutdown::reset();
        let mut buf = vec![0u64; 1024];
        let (tx, rx) = mpsc::sync_channel::<TuiEvent>(256);
        let state = make_test_state(&[Pattern::SolidBits]);
        let sink = make_sink();

        run_region_worker(
            &mut buf,
            &[Pattern::SolidBits],
            1,
            false,
            0,
            &state,
            &tx,
            None,
            &sink,
        );

        let mut found_done = false;
        while let Ok(event) = rx.try_recv() {
            if let TuiEvent::RegionDone(idx) = event {
                check!(idx == 0);
                found_done = true;
            }
        }
        assert!(found_done, "expected TuiEvent::RegionDone");
    }

    #[test]
    #[serial]
    fn worker_progress_reaches_completion() {
        shutdown::reset();
        let mut buf = vec![0u64; 1024];
        let (tx, _rx) = mpsc::sync_channel::<TuiEvent>(256);
        let state = make_test_state(&[Pattern::SolidBits]);
        let sink = make_sink();

        run_region_worker(
            &mut buf,
            &[Pattern::SolidBits],
            1,
            false,
            0,
            &state,
            &tx,
            None,
            &sink,
        );

        check!(state.progress_bp.load(Ordering::Relaxed) == 10000);
    }

    #[test]
    #[serial]
    fn worker_zero_errors_on_clean_memory() {
        shutdown::reset();
        let mut buf = vec![0u64; 1024];
        let (tx, _rx) = mpsc::sync_channel::<TuiEvent>(256);
        let state = make_test_state(Pattern::ALL);
        let sink = make_sink();

        run_region_worker(
            &mut buf,
            Pattern::ALL,
            1,
            false,
            0,
            &state,
            &tx,
            None,
            &sink,
        );

        check!(state.error_count.load(Ordering::Relaxed) == 0);
    }

    #[test]
    #[serial]
    fn worker_respects_quit_flag() {
        shutdown::reset();
        shutdown::request_quit(shutdown::QuitReason::UserQuit);
        let mut buf = vec![0u64; 1024];
        let (tx, rx) = mpsc::sync_channel::<TuiEvent>(256);
        let state = make_test_state(Pattern::ALL);
        let sink = make_sink();

        run_region_worker(
            &mut buf,
            Pattern::ALL,
            100,
            false,
            0,
            &state,
            &tx,
            None,
            &sink,
        );

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        let done_count = events
            .iter()
            .filter(|e| matches!(e, TuiEvent::RegionDone(_)))
            .count();
        check!(done_count == 1);
    }

    #[test]
    #[serial]
    fn worker_multi_pass() {
        shutdown::reset();
        let mut buf = vec![0u64; 1024];
        let (tx, _rx) = mpsc::sync_channel::<TuiEvent>(256);
        let state = make_test_state(&[Pattern::SolidBits]);
        let sink = make_sink();

        run_region_worker(
            &mut buf,
            &[Pattern::SolidBits],
            3,
            false,
            0,
            &state,
            &tx,
            None,
            &sink,
        );

        check!(state.error_count.load(Ordering::Relaxed) == 0);
        check!(state.progress_bp.load(Ordering::Relaxed) == 10000);
    }
}
