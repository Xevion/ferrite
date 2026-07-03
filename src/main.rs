#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

#[cfg(feature = "tui")]
use std::io::IsTerminal;

use anyhow::{Context, Result};
use clap::Parser;

use ferrite::events::{EventRx, RunEvent};
use ferrite::headless::HeadlessPrinter;
use ferrite::ndjson::NdjsonEventWriter;
use ferrite::pattern::Pattern;
use ferrite::phys::PhysResolver;
use ferrite::results::{ResultsDoc, ResultsRenderer, TableRenderer};
use ferrite::runner;
use ferrite::shutdown;
#[cfg(feature = "tui")]
use ferrite::tui::run::{TuiTestSetup, run_tui_mode};

mod cli;
#[cfg(feature = "tui")]
use cli::TuiMode;
use cli::{Cli, OutputConfig, OutputFormat, check_privileges, setup_test};

fn main() -> Result<()> {
    let mut cli = Cli::parse();
    let shutdown_handle = shutdown::install_signal_handlers()?;
    shutdown::install_panic_hook();

    let output = cli.resolve_output()?;

    // Apply color override globally via owo-colors.
    // When color_enabled is false (Never, Auto-unsupported, or JSON format), force off.
    // When Always, force on. Auto-supported: no override, let owo-colors auto-detect.
    if !output.color_enabled {
        owo_colors::set_override(false);
    } else if matches!(cli.color, cli::ColorMode::Always) {
        owo_colors::set_override(true);
    }

    // Init tracing early with stderr output so privilege warnings are visible.
    // The TUI path hot-swaps to its channel writer via the reload handle.
    let tracing_handle = init_tracing();

    let need_phys = !cli.no_phys;
    check_privileges(cli.requested_bytes_estimate(), need_phys);

    // Load (or initialize) the cross-run coverage store before the run so
    // cumulative coverage is reported up front.
    let coverage_ctx = open_coverage_store(&cli)?;

    let patterns = if cli.patterns.is_empty() {
        Pattern::ALL.to_vec()
    } else {
        std::mem::take(&mut cli.patterns)
    };

    let workers = cli.parallel.resolve();
    if workers > 1
        && let Err(e) = rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .build_global()
    {
        tracing::warn!("failed to configure {workers}-thread rayon pool: {e}");
    }
    let parallel = workers > 1;

    #[cfg(feature = "tui")]
    {
        let use_tui = match cli.tui {
            TuiMode::Always => true,
            TuiMode::Never => false,
            TuiMode::Auto => std::io::stdout().is_terminal(),
        };

        if use_tui {
            if output.format == OutputFormat::Json {
                anyhow::bail!(
                    "--format json is not supported with TUI mode. \
                     Use --tui never for JSON output."
                );
            }

            let events_writer = open_events_writer(&output)?;

            let cull = cull_ranges(&cli, coverage_ctx.as_ref());
            let s = setup_test(&cli, cull.as_deref())?;
            let size = s.buffer.len();
            let run_ranges = s
                .resolver
                .as_ref()
                .map(|r| ferrite::coverage::compact_pfns(r.pfns()));
            let tui_setup = TuiTestSetup {
                buffer: s.buffer,
                resolver: s.resolver,
                map_stats: s.map_stats,
                compaction_guard: s.compaction_guard,
            };
            let mut results = run_tui_mode(
                size,
                cli.passes,
                workers,
                tui_setup,
                patterns,
                &tracing_handle,
                events_writer,
            )?;

            ferrite::error_analysis::analyze(&mut results);
            let covered = finalize_coverage(coverage_ctx, run_ranges, &mut results);
            attach_gap_classification(covered, &mut results);
            render_results(&output, &results, cli.units, true);

            let code = shutdown::exit_code(results.total_failures);
            shutdown_handle.shutdown();
            if code != 0 {
                std::process::exit(code);
            }
            return Ok(());
        }
    }

    // Non-TUI path: handle is no longer needed (stderr layer stays).
    drop(tracing_handle);

    let result = run_non_tui(&cli, &patterns, &output, workers, parallel, coverage_ctx);
    shutdown_handle.shutdown();
    result
}

/// A loaded (or freshly initialized) coverage store plus its file path.
struct CoverageCtx {
    store: ferrite::coverage::CoverageStore,
    path: std::path::PathBuf,
}

/// The covered set the `--cull` sieve should hold hostage, when culling is
/// requested. clap guarantees `--cull` implies `--coverage-file`.
fn cull_ranges(cli: &Cli, ctx: Option<&CoverageCtx>) -> Option<Vec<ferrite::coverage::PfnRange>> {
    cli.cull
        .then(|| ctx.map(|c| c.store.ranges.clone()).unwrap_or_default())
}

/// Open the `--coverage-file` store when configured: load and validate an
/// existing file (reporting cumulative coverage) or initialize a new store.
fn open_coverage_store(cli: &Cli) -> Result<Option<CoverageCtx>> {
    let Some(path) = cli.coverage_file.clone() else {
        return Ok(None);
    };
    if cli.no_phys {
        anyhow::bail!("--coverage-file requires physical address resolution (remove --no-phys)");
    }
    let fingerprint = ferrite::sysmem::machine_fingerprint()
        .context("cannot fingerprint machine memory for coverage tracking")?;
    let loaded = ferrite::coverage::CoverageStore::load(&path, fingerprint)
        .with_context(|| format!("failed to load coverage file: {}", path.display()))?;
    let store = if let Some(store) = loaded {
        let covered = store.covered_bytes();
        let installed = ferrite::sysmem::installed_ram().map_or(0, |r| r.bytes);
        let pct = if installed > 0 {
            covered as f64 / installed as f64 * 100.0
        } else {
            0.0
        };
        tracing::info!(
            "cumulative coverage: {} / {} ({pct:.1}%) across {} previous run(s)",
            ferrite::units::format_size(covered as usize),
            ferrite::units::format_size(installed as usize),
            store.runs.len(),
        );
        store
    } else {
        tracing::info!("starting new coverage file: {}", path.display());
        ferrite::coverage::CoverageStore::new(fingerprint)
    };
    Ok(Some(CoverageCtx { store, path }))
}

/// Merge a completed run into the coverage store, persist it, and attach
/// cumulative stats to the results. Interrupted runs are not merged -- their
/// frames were not tested by every selected pattern.
///
/// Returns the covered set for gap classification: the store's cumulative
/// ranges when one is active, this run's frames otherwise. `None` when the
/// run cannot count toward coverage (unresolved or interrupted).
fn finalize_coverage(
    ctx: Option<CoverageCtx>,
    run_ranges: Option<Vec<ferrite::coverage::PfnRange>>,
    results: &mut ferrite::runner::RunResults,
) -> Option<Vec<ferrite::coverage::PfnRange>> {
    let Some(ranges) = run_ranges else {
        if ctx.is_some() {
            tracing::warn!("coverage store not updated: physical address resolution unavailable");
        }
        return None;
    };
    let interrupted = results
        .passes
        .iter()
        .flat_map(|p| &p.pattern_results)
        .any(|r| r.interrupted);
    if interrupted {
        if ctx.is_some() {
            tracing::warn!("coverage store not updated: run was interrupted");
        }
        return None;
    }
    let Some(mut ctx) = ctx else {
        return Some(ranges);
    };

    let patterns = results
        .config
        .patterns
        .iter()
        .map(ToString::to_string)
        .collect();
    let delta = ctx.store.record_run(
        &ranges,
        jiff::Timestamp::now(),
        patterns,
        results.config.passes,
        results.total_failures as u64,
    );
    if let Err(e) = ctx.store.save(&ctx.path) {
        tracing::warn!("failed to save coverage file: {e}");
    }
    results
        .coverage
        .attach_cumulative(ferrite::sysmem::Cumulative {
            new_bytes: delta.new_bytes,
            cumulative_bytes: delta.cumulative_bytes,
            runs: delta.runs,
        });
    Some(std::mem::take(&mut ctx.store.ranges))
}

/// Classify what the untested remainder of installed RAM is doing and attach
/// the breakdown to the results. Requires root (`/proc/kpageflags`); silently
/// skipped otherwise.
fn attach_gap_classification(
    covered: Option<Vec<ferrite::coverage::PfnRange>>,
    results: &mut ferrite::runner::RunResults,
) {
    if let Some(covered) = covered
        && let Some(report) = ferrite::gap::classify_system_gaps(&covered)
    {
        results.coverage.attach_gap(report);
    }
}

/// Render final results to stdout based on output configuration.
///
/// When `full_table` is true, the table renderer includes per-pattern detail
/// (used after TUI exit, where no live output was shown). When false, only
/// the summary and error analysis are rendered (after `HeadlessPrinter`
/// already streamed live results).
fn render_results(
    output: &OutputConfig,
    results: &ferrite::runner::RunResults,
    unit_system: ferrite::units::UnitSystem,
    full_table: bool,
) {
    let doc = ResultsDoc::from_results(results);
    match output.format {
        OutputFormat::Json => {
            ferrite::results::JsonRenderer
                .render(&doc, &mut std::io::stdout())
                .unwrap_or_else(|e| eprintln!("warning: failed to render results: {e}"));
        }
        OutputFormat::Table => {
            let renderer = if full_table {
                TableRenderer::full(unit_system)
            } else {
                TableRenderer::new(unit_system)
            };
            renderer
                .render(&doc, &mut std::io::stdout())
                .unwrap_or_else(|e| eprintln!("warning: failed to render results: {e}"));
        }
    }
}

/// Open the NDJSON event writer for `--events <file>`, if configured.
fn open_events_writer(output: &OutputConfig) -> Result<Option<NdjsonEventWriter>> {
    output
        .events_file
        .as_deref()
        .map(|p| {
            let path_str = p
                .to_str()
                .expect("events_file path validated as UTF-8 in resolve_output");
            NdjsonEventWriter::from_path(path_str)
                .with_context(|| format!("failed to open events file: {}", p.display()))
        })
        .transpose()
}

/// Consume events from the runner and drive human-readable output + JSON emission.
///
/// Runs on a dedicated thread. The [`HeadlessPrinter`] handles human-readable
/// text while [`NdjsonEventWriter`] handles JSON emission (when present).
fn consume_headless_events(
    rx: &EventRx,
    printer: &mut HeadlessPrinter<std::io::Stdout>,
    stdout_ndjson: &mut Option<NdjsonEventWriter>,
    events_ndjson: &mut Option<NdjsonEventWriter>,
    suppress_human: bool,
) {
    while let Ok(event) = rx.recv() {
        if !suppress_human {
            printer.handle_event(&event);
        }
        if let Some(w) = stdout_ndjson.as_mut() {
            w.handle_event(&event);
        }
        if let Some(w) = events_ndjson.as_mut() {
            w.handle_event(&event);
        }
        if matches!(event, RunEvent::RunComplete) {
            break;
        }
    }
}

/// Non-TUI mode: headless output with tracing to stderr.
fn run_non_tui(
    cli: &Cli,
    patterns: &[Pattern],
    output: &OutputConfig,
    workers: usize,
    parallel: bool,
    coverage_ctx: Option<CoverageCtx>,
) -> Result<()> {
    let cull = cull_ranges(cli, coverage_ctx.as_ref());
    let mut setup = setup_test(cli, cull.as_deref())?;
    let size = setup.buffer.len();
    let run_ranges = setup
        .resolver
        .as_ref()
        .map(|r| ferrite::coverage::compact_pfns(r.pfns()));

    let (tx, rx) = ferrite::events::event_bus();

    // Emit global events before the run
    let _ = tx.send(RunEvent::RunStart {
        size,
        passes: cli.passes,
        patterns: patterns.to_vec(),
        workers,
    });

    if let Some(ref stats) = setup.map_stats {
        let _ = tx.send(RunEvent::MapInfo {
            stats: stats.clone(),
        });
    }

    let unit_system = cli.units;
    let format = output.format;

    // --format json without --events <file>: NDJSON events stream to stdout
    let json_to_stdout = format == OutputFormat::Json && output.events_file.is_none();

    // Suppress human output when format is JSON — stdout is a JSON-only surface
    let suppress_human = format == OutputFormat::Json;

    // NDJSON writer for stdout (live events when --format json, no --events file)
    let mut stdout_ndjson = if json_to_stdout {
        Some(NdjsonEventWriter::new(Box::new(std::io::stdout())))
    } else {
        None
    };

    // NDJSON writer for --events <file>
    let mut events_ndjson = open_events_writer(output)?;

    // Consumer thread drives HeadlessPrinter (human) + optional NDJSON writers.
    let consumer = std::thread::spawn(move || {
        let mut printer = HeadlessPrinter::new(std::io::stdout(), unit_system);
        consume_headless_events(
            &rx,
            &mut printer,
            &mut stdout_ndjson,
            &mut events_ndjson,
            suppress_human,
        );
        (printer, stdout_ndjson, events_ndjson)
    });

    let run_start = std::time::Instant::now();
    let pass_results = runner::run(
        setup.buffer.as_u64_slice_mut(),
        patterns,
        cli.passes,
        parallel,
        &tx,
        setup
            .resolver
            .as_ref()
            .map(|r| r as &(dyn PhysResolver + Sync)),
        &|_| {},
    )
    .context("pattern execution failed")?;
    let run_elapsed = run_start.elapsed();

    let _ = tx.send(RunEvent::RunComplete);
    drop(tx);

    let (_printer, mut stdout_ndjson, mut events_ndjson) =
        consumer.join().expect("event consumer thread panicked");

    let config = ferrite::runner::RunConfig {
        size,
        passes: cli.passes,
        patterns: patterns.to_vec(),
        workers,
    };
    let mut results = ferrite::runner::RunResults::from_passes(pass_results, config, run_elapsed);
    results.coverage = ferrite::sysmem::coverage_for(setup.map_stats.as_ref());

    ferrite::error_analysis::analyze(&mut results);
    let covered = finalize_coverage(coverage_ctx, run_ranges, &mut results);
    attach_gap_classification(covered, &mut results);

    // Write run_complete to whichever NDJSON writers are active
    if let Some(w) = stdout_ndjson.as_mut() {
        w.write_run_complete(
            cli.passes,
            results.total_failures,
            run_elapsed,
            results.coverage,
        );
    }
    if let Some(w) = events_ndjson.as_mut() {
        w.write_run_complete(
            cli.passes,
            results.total_failures,
            run_elapsed,
            results.coverage,
        );
    }

    render_results(output, &results, cli.units, false);

    let code = shutdown::exit_code(results.total_failures);
    if code != 0 {
        std::process::exit(code);
    }

    Ok(())
}

type BoxedTracingLayer =
    Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync>;

/// Initialize the global tracing subscriber with a reloadable layer.
///
/// Starts with human-readable output on stderr. The returned handle can be used
/// to hot-swap the layer (e.g. to route tracing through the TUI channel).
fn init_tracing()
-> tracing_subscriber::reload::Handle<BoxedTracingLayer, tracing_subscriber::Registry> {
    use tracing_subscriber::prelude::*;

    let initial: BoxedTracingLayer =
        Box::new(tracing_subscriber::fmt::layer().with_writer(std::io::stderr));
    let (reload_layer, handle) = tracing_subscriber::reload::Layer::new(initial);
    tracing_subscriber::registry().with(reload_layer).init();
    handle
}
