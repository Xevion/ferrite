#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

#[cfg(feature = "tui")]
use std::io::IsTerminal;

use clap::Parser;
#[cfg(feature = "tui")]
use snafu::whatever;
use snafu::{ResultExt, Whatever};

use ferrite::events::{EventRx, RunEvent};
use ferrite::headless::HeadlessPrinter;
use ferrite::ndjson::NdjsonEventWriter;
use ferrite::pattern::Pattern;
use ferrite::physmem::lifecycle::{self, CoverageCtx};
use ferrite::physmem::phys::PhysResolver;
use ferrite::results::{ResultsDoc, ResultsRenderer, TableRenderer};
use ferrite::runner;
use ferrite::shutdown;
#[cfg(feature = "tui")]
use ferrite::tui::run::{TuiTestSetup, run_tui_mode};

mod cli;
mod devmem_run;
#[cfg(feature = "tui")]
use cli::TuiMode;
use cli::{Cli, OutputConfig, OutputFormat, SetupOutcome, check_privileges, setup_test};

/// Application-level result defaulting to [`snafu::Whatever`] for loose,
/// message-based errors; the error type stays overridable for callers that
/// need a specific one.
type Result<T, E = Whatever> = std::result::Result<T, E>;

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
    let coverage_ctx = CoverageCtx::open(cli.coverage_file.as_deref(), cli.no_phys)?;

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

    // /dev/mem targeted testing is a distinct, always-headless backend: it maps
    // chosen physical ranges rather than anonymous memory.
    if let Some(target) = cli.devmem {
        drop(tracing_handle);
        let result = devmem_run::run(&cli, target, &patterns, workers, parallel);
        shutdown_handle.shutdown();
        return result;
    }

    #[cfg(feature = "tui")]
    {
        let use_tui = match cli.tui {
            TuiMode::Always => true,
            TuiMode::Never => false,
            TuiMode::Auto => std::io::stdout().is_terminal(),
        };

        if use_tui {
            if output.format == OutputFormat::Json {
                whatever!(
                    "--format json is not supported with TUI mode. \
                     Use --tui never for JSON output."
                );
            }

            let events_writer = open_events_writer(&output)?;

            let cull = lifecycle::cull_ranges(cli.cull, coverage_ctx.as_ref());
            let s = match setup_test(&cli, cull.as_deref())? {
                SetupOutcome::Ready(s) => s,
                SetupOutcome::CullCeiling => {
                    lifecycle::report_cull_ceiling(
                        coverage_ctx.as_ref(),
                        cull.as_deref().unwrap_or(&[]),
                        output.format == OutputFormat::Table,
                        cli.units,
                    );
                    shutdown_handle.shutdown();
                    return Ok(());
                }
            };
            let size = s.buffer.len();
            let run_ranges = s
                .resolver
                .as_ref()
                .map(|r| ferrite::physmem::pfn::compact_pfns(r.pfns()));
            let tui_setup = TuiTestSetup {
                buffer: s.buffer,
                resolver: s.resolver,
                map_stats: s.map_stats,
                compaction_guard: s.compaction_guard,
            };
            let run_output = run_tui_mode(
                size,
                cli.passes,
                workers,
                tui_setup,
                patterns,
                &tracing_handle,
                events_writer,
            )?;

            let results = runner::execute_run(
                run_output.pass_results,
                run_output.config,
                run_output.elapsed,
                run_output.coverage,
                coverage_ctx,
                run_ranges,
            );
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
                .with_whatever_context(|_| format!("failed to open events file: {}", p.display()))
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
    let cull = lifecycle::cull_ranges(cli.cull, coverage_ctx.as_ref());
    let mut setup = match setup_test(cli, cull.as_deref())? {
        SetupOutcome::Ready(s) => s,
        SetupOutcome::CullCeiling => {
            lifecycle::report_cull_ceiling(
                coverage_ctx.as_ref(),
                cull.as_deref().unwrap_or(&[]),
                output.format == OutputFormat::Table,
                cli.units,
            );
            return Ok(());
        }
    };
    let size = setup.buffer.len();
    let run_ranges = setup
        .resolver
        .as_ref()
        .map(|r| ferrite::physmem::pfn::compact_pfns(r.pfns()));

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
    .whatever_context("pattern execution failed")?;
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
    let coverage = ferrite::physmem::sysmem::coverage_for(setup.map_stats.as_ref());
    let results = runner::execute_run(
        pass_results,
        config,
        run_elapsed,
        coverage,
        coverage_ctx,
        run_ranges,
    );

    // Write run_complete to whichever NDJSON writers are active. This happens
    // after the shared results tail, so its coverage carries cumulative stats.
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
