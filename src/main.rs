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
    check_privileges(cli.size, need_phys);

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

            let s = setup_test(&cli)?;
            let tui_setup = TuiTestSetup {
                buffer: s.buffer,
                resolver: s.resolver,
                map_stats: s.map_stats,
                compaction_guard: s.compaction_guard,
            };
            let mut results = run_tui_mode(
                cli.size,
                cli.passes,
                workers,
                tui_setup,
                patterns,
                &tracing_handle,
                events_writer,
            )?;

            ferrite::error_analysis::analyze(&mut results);
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

    let result = run_non_tui(&cli, &patterns, &output, workers, parallel);
    shutdown_handle.shutdown();
    result
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
) -> Result<()> {
    let mut setup = setup_test(cli)?;

    let (tx, rx) = ferrite::events::event_bus();

    // Emit global events before the run
    let _ = tx.send(RunEvent::RunStart {
        size: cli.size,
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
        size: cli.size,
        passes: cli.passes,
        patterns: patterns.to_vec(),
        workers,
    };
    let mut results = ferrite::runner::RunResults::from_passes(pass_results, config, run_elapsed);

    ferrite::error_analysis::analyze(&mut results);

    // Write run_complete to whichever NDJSON writers are active
    if let Some(w) = stdout_ndjson.as_mut() {
        w.write_run_complete(cli.passes, results.total_failures, run_elapsed);
    }
    if let Some(w) = events_ndjson.as_mut() {
        w.write_run_complete(cli.passes, results.total_failures, run_elapsed);
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
