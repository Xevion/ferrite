#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

#[cfg(feature = "tui")]
use std::io::IsTerminal;

use anyhow::{Context, Result};
use clap::Parser;

use ferrite::events::{EventRx, RegionEvent, RunEvent};
use ferrite::headless::HeadlessPrinter;
use ferrite::output::OutputSink;
use ferrite::pattern::Pattern;
use ferrite::phys::PhysResolver;
use ferrite::results::{ResultsDoc, ResultsRenderer, TableRenderer};
use ferrite::runner;
use ferrite::shutdown;
#[cfg(feature = "tui")]
use ferrite::tui::run::{TuiTestSetup, run_tui_mode, setup_tracing};

mod cli;
#[cfg(feature = "tui")]
use cli::TuiMode;
use cli::{Cli, check_privileges, setup_test};

fn main() -> Result<()> {
    let mut cli = Cli::parse();
    let shutdown_handle = shutdown::install_signal_handlers()?;
    shutdown::install_panic_hook();

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
                false,
            );
        }
    }

    let sink = if let Some(ref json_path) = cli.json {
        OutputSink::json(json_path, cli.units).context("failed to open JSON output")?
    } else {
        OutputSink::human(cli.units)
    };

    let result = run_non_tui(&cli, &patterns, sink);
    shutdown_handle.shutdown();
    result
}

/// Consume events from the runner and drive human-readable output + JSON emission.
///
/// Runs on a dedicated thread. The [`HeadlessPrinter`] handles human-readable
/// text while [`OutputSink`] handles JSON emission (when active).
fn consume_headless_events(
    rx: &EventRx,
    printer: &mut HeadlessPrinter<std::io::Stdout>,
    sink: &mut OutputSink,
) {
    while let Ok(event) = rx.recv() {
        // Human-readable output
        printer.handle_event(&event);

        // JSON emission (no-ops when sink is Human)
        match &event {
            RunEvent::MapInfo { stats } => sink.emit_map_info(stats),
            RunEvent::Region(_, RegionEvent::PassStart { pass, total_passes }) => {
                sink.emit_pass_start(*pass, *total_passes);
            }
            RunEvent::Region(_, RegionEvent::TestStart { pattern, pass }) => {
                sink.emit_test_start(*pattern, *pass);
            }
            RunEvent::Region(
                _,
                RegionEvent::Progress {
                    pattern,
                    pass,
                    sub_pass,
                    total,
                },
            ) => sink.emit_progress(*pattern, *pass, *sub_pass, *total),
            RunEvent::Region(
                _,
                RegionEvent::TestComplete {
                    pattern,
                    pass,
                    elapsed,
                    bytes,
                    failures,
                },
            ) => sink.emit_test_complete(*pattern, *pass, *elapsed, *bytes, failures),
            RunEvent::Region(
                _,
                RegionEvent::PassComplete {
                    pass,
                    failures,
                    elapsed,
                },
            ) => sink.emit_pass_complete(*pass, *failures, *elapsed),
            RunEvent::Region(_, RegionEvent::EccDeltas { pass, deltas }) => {
                sink.emit_ecc_deltas(*pass, deltas);
            }
            _ => {}
        }

        if matches!(event, RunEvent::RunComplete) {
            break;
        }
    }
}

/// Non-TUI mode: headless output with tracing to stderr.
fn run_non_tui(cli: &Cli, patterns: &[Pattern], mut sink: OutputSink) -> Result<()> {
    #[cfg(feature = "tui")]
    setup_tracing(sink.is_json(), None);

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

    let unit_system = sink.unit_system();

    // Consumer thread drives HeadlessPrinter + OutputSink from events
    let consumer = std::thread::spawn(move || {
        let mut printer = HeadlessPrinter::new(std::io::stdout(), unit_system);
        consume_headless_events(&rx, &mut printer, &mut sink);
        (printer, sink)
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

    let (_printer, mut sink) = consumer.join().expect("event consumer thread panicked");

    let config = ferrite::runner::RunConfig {
        size: cli.size,
        passes: cli.passes,
        patterns: patterns.to_vec(),
        regions: 1,
        parallel: !cli.sequential,
    };
    let mut results = ferrite::runner::RunResults::from_passes(pass_results, config, run_elapsed);

    ferrite::error_analysis::analyze(&mut results);

    sink.emit_summary(cli.passes, results.total_failures, run_elapsed);

    let doc = ResultsDoc::from_results(&results);
    let renderer = TableRenderer::new(cli.units);
    renderer
        .render(&doc, &mut std::io::stdout())
        .unwrap_or_else(|e| eprintln!("warning: failed to render results: {e}"));

    let code = shutdown::exit_code(results.total_failures);
    if code != 0 {
        std::process::exit(code);
    }

    Ok(())
}
