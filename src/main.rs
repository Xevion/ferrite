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
use cli::{Cli, check_privileges, setup_test};

fn main() -> Result<()> {
    let mut cli = Cli::parse();
    let shutdown_handle = shutdown::install_signal_handlers()?;
    shutdown::install_panic_hook();

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

        if use_tui && cli.json.is_some() {
            anyhow::bail!(
                "--json is not yet supported with --tui. Use --tui never for JSON output."
            );
        }

        if use_tui {
            let s = setup_test(&cli)?;
            let tui_setup = TuiTestSetup {
                region: s.region,
                resolver: s.resolver,
                map_stats: s.map_stats,
                compaction_guard: s.compaction_guard,
            };
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

    let ndjson_writer = cli
        .json
        .as_deref()
        .map(NdjsonEventWriter::from_path)
        .transpose()
        .context("failed to open JSON output")?;

    let result = run_non_tui(&cli, &patterns, ndjson_writer);
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
    ndjson: &mut Option<NdjsonEventWriter>,
    json_mode: bool,
) {
    while let Ok(event) = rx.recv() {
        if !json_mode {
            printer.handle_event(&event);
        }
        if let Some(w) = ndjson.as_mut() {
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
    mut ndjson_writer: Option<NdjsonEventWriter>,
) -> Result<()> {
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
    let json_mode = ndjson_writer.is_some();

    // Consumer thread drives HeadlessPrinter (human) + optional NdjsonEventWriter (JSON).
    // When JSON mode is active, suppress human output — stdout is exclusively NDJSON.
    let consumer = std::thread::spawn(move || {
        let mut printer = HeadlessPrinter::new(std::io::stdout(), unit_system);
        consume_headless_events(&rx, &mut printer, &mut ndjson_writer, json_mode);
        (printer, ndjson_writer)
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

    let (_printer, mut ndjson_writer) = consumer.join().expect("event consumer thread panicked");

    let config = ferrite::runner::RunConfig {
        size: cli.size,
        passes: cli.passes,
        patterns: patterns.to_vec(),
        regions: 1,
        parallel: !cli.sequential,
    };
    let mut results = ferrite::runner::RunResults::from_passes(pass_results, config, run_elapsed);

    ferrite::error_analysis::analyze(&mut results);

    if let Some(w) = ndjson_writer.as_mut() {
        w.write_run_complete(cli.passes, results.total_failures, run_elapsed);
    } else {
        let doc = ResultsDoc::from_results(&results);
        let renderer = TableRenderer::new(cli.units);
        renderer
            .render(&doc, &mut std::io::stdout())
            .unwrap_or_else(|e| eprintln!("warning: failed to render results: {e}"));
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
