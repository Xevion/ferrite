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

    #[cfg(feature = "tui")]
    {
        let use_tui = match cli.tui {
            TuiMode::Always => true,
            TuiMode::Never => false,
            TuiMode::Auto => std::io::stdout().is_terminal(),
        };

        if use_tui {
            let s = setup_test(&cli)?;
            let tui_setup = TuiTestSetup {
                region: s.region,
                resolver: s.resolver,
                map_stats: s.map_stats,
                compaction_guard: s.compaction_guard,
            };
            // TUI + --log <file>: events saved to file while TUI renders
            let ndjson_writer = output
                .log_file
                .as_deref()
                .map(|p| NdjsonEventWriter::from_path(p.to_str().unwrap_or("")))
                .transpose()
                .context("failed to open log file")?;
            drop(ndjson_writer); // TODO: wire into TUI path in unified runner
            return run_tui_mode(
                cli.size,
                cli.passes,
                cli.regions,
                cli.sequential,
                tui_setup,
                patterns,
                &tracing_handle,
            );
        }
    }

    // Non-TUI path: handle is no longer needed (stderr layer stays).
    drop(tracing_handle);

    let result = run_non_tui(&cli, &patterns, &output);
    shutdown_handle.shutdown();
    result
}

/// Consume events from the runner and drive human-readable output + JSON emission.
///
/// Runs on a dedicated thread. The [`HeadlessPrinter`] handles human-readable
/// text while [`NdjsonEventWriter`] handles JSON emission (when present).
fn consume_headless_events(
    rx: &EventRx,
    printer: &mut HeadlessPrinter<std::io::Stdout>,
    stdout_ndjson: &mut Option<NdjsonEventWriter>,
    log_ndjson: &mut Option<NdjsonEventWriter>,
    suppress_human: bool,
) {
    while let Ok(event) = rx.recv() {
        if !suppress_human {
            printer.handle_event(&event);
        }
        if let Some(w) = stdout_ndjson.as_mut() {
            w.handle_event(&event);
        }
        if let Some(w) = log_ndjson.as_mut() {
            w.handle_event(&event);
        }
        if matches!(event, RunEvent::RunComplete) {
            break;
        }
    }
}

/// Non-TUI mode: headless output with tracing to stderr.
fn run_non_tui(cli: &Cli, patterns: &[Pattern], output: &OutputConfig) -> Result<()> {
    let mut setup = setup_test(cli)?;

    let (tx, rx) = ferrite::events::event_bus();

    // Emit global events before the run
    let _ = tx.send(RunEvent::RunStart {
        size: cli.size,
        passes: cli.passes,
        patterns: patterns.to_vec(),
        regions: 1,
        parallel: !cli.sequential,
    });

    if let Some(ref stats) = setup.map_stats {
        let _ = tx.send(RunEvent::MapInfo {
            stats: stats.clone(),
        });
    }

    let unit_system = cli.units;
    let format = output.format;

    // --format json without --log <file>: NDJSON events stream to stdout
    let json_to_stdout = format == OutputFormat::Json && output.log_file.is_none();
    // --format json + --log <file>: events go to file, only final results on stdout
    let json_events_to_file = format == OutputFormat::Json && output.log_file.is_some();

    // Suppress human output when stdout is used for JSON
    let suppress_human = format == OutputFormat::Json;

    // NDJSON writer for stdout (live events when --format json, no --log file)
    let mut stdout_ndjson = if json_to_stdout {
        Some(NdjsonEventWriter::new(Box::new(std::io::stdout())))
    } else {
        None
    };

    // NDJSON writer for --log <file>
    let mut log_ndjson = output
        .log_file
        .as_deref()
        .map(|p| NdjsonEventWriter::from_path(p.to_str().unwrap_or("")))
        .transpose()
        .context("failed to open log file")?;

    // Consumer thread drives HeadlessPrinter (human) + optional NDJSON writers.
    let consumer = std::thread::spawn(move || {
        let mut printer = HeadlessPrinter::new(std::io::stdout(), unit_system);
        consume_headless_events(
            &rx,
            &mut printer,
            &mut stdout_ndjson,
            &mut log_ndjson,
            suppress_human,
        );
        (printer, stdout_ndjson, log_ndjson)
    });

    let run_start = std::time::Instant::now();
    let pass_results = runner::run(
        setup.region.as_u64_slice_mut(),
        0,
        patterns,
        cli.passes,
        !cli.sequential,
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

    let (_printer, mut stdout_ndjson, mut log_ndjson) =
        consumer.join().expect("event consumer thread panicked");

    let config = ferrite::runner::RunConfig {
        size: cli.size,
        passes: cli.passes,
        patterns: patterns.to_vec(),
        regions: 1,
        parallel: !cli.sequential,
    };
    let mut results = ferrite::runner::RunResults::from_passes(pass_results, config, run_elapsed);

    ferrite::error_analysis::analyze(&mut results);

    // Write run_complete to whichever NDJSON writers are active
    if let Some(w) = stdout_ndjson.as_mut() {
        w.write_run_complete(cli.passes, results.total_failures, run_elapsed);
    }
    if let Some(w) = log_ndjson.as_mut() {
        w.write_run_complete(cli.passes, results.total_failures, run_elapsed);
    }

    // Final results rendering
    match format {
        OutputFormat::Json if json_events_to_file => {
            // Events went to log file; render final JSON results to stdout
            let doc = ResultsDoc::from_results(&results);
            let renderer = ferrite::results::JsonRenderer;
            renderer
                .render(&doc, &mut std::io::stdout())
                .unwrap_or_else(|e| eprintln!("warning: failed to render results: {e}"));
        }
        OutputFormat::Json => {
            // Events already streamed to stdout; run_complete event is the final record
        }
        OutputFormat::Table => {
            let doc = ResultsDoc::from_results(&results);
            let renderer = TableRenderer::new(cli.units);
            renderer
                .render(&doc, &mut std::io::stdout())
                .unwrap_or_else(|e| eprintln!("warning: failed to render results: {e}"));
        }
    }

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
