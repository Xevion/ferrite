#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

#[cfg(feature = "tui")]
use std::io::IsTerminal;

use anyhow::{Context, Result};
use clap::Parser;

use ferrite::events::{EventRx, RegionEvent, RunEvent};
use ferrite::output::OutputSink;
use ferrite::pattern::Pattern;
use ferrite::phys::PhysResolver;
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

    let sink = if let Some(ref json_path) = cli.json {
        OutputSink::json(json_path, cli.units).context("failed to open JSON output")?
    } else {
        OutputSink::human(cli.units)
    };

    #[cfg(feature = "tui")]
    {
        let use_tui = match cli.tui {
            TuiMode::Always => true,
            TuiMode::Never => false,
            TuiMode::Auto => std::io::stdout().is_terminal(),
        };

        // JSON-to-stdout + TUI = both claim stdout
        if use_tui
            && let Some(ref path) = cli.json
            && (path == "-" || path.is_empty())
        {
            anyhow::bail!(
                "--json to stdout conflicts with --tui (both use stdout). \
                 Use --json <file> or --tui never."
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
                sink,
            );
        }
    }

    let result = run_non_tui(&cli, &patterns, sink);
    shutdown_handle.shutdown();
    result
}

/// Consume events from the runner and drive [`OutputSink`] for live output.
///
/// Runs on a dedicated thread. Returns the sink when the channel disconnects
/// or `RunComplete` is received.
fn consume_headless_events(rx: &EventRx, sink: &mut OutputSink) {
    let mut total_passes: usize = 0;

    while let Ok(event) = rx.recv() {
        match event {
            RunEvent::RunStart {
                size,
                passes,
                ref patterns,
                parallel,
                ..
            } => {
                total_passes = passes;
                sink.print_banner(size, passes, patterns.len(), parallel);
            }
            RunEvent::MapInfo { ref stats } => {
                sink.emit_map_info(stats);
                sink.print_map_info(stats);
            }
            RunEvent::Region(
                _,
                RegionEvent::PassStart {
                    pass,
                    total_passes: tp,
                },
            ) => {
                sink.emit_pass_start(pass, tp);
            }
            RunEvent::Region(_, RegionEvent::TestStart { pattern, pass }) => {
                sink.emit_test_start(pattern, pass);
            }
            RunEvent::Region(
                _,
                RegionEvent::Progress {
                    pattern,
                    pass,
                    sub_pass,
                    total,
                },
            ) => {
                sink.emit_progress(pattern, pass, sub_pass, total);
            }
            RunEvent::Region(
                _,
                RegionEvent::TestComplete {
                    pattern,
                    pass,
                    elapsed,
                    bytes,
                    ref failures,
                },
            ) => {
                sink.emit_test_complete(pattern, pass, elapsed, bytes, failures);
                sink.print_test_result(pattern, elapsed, bytes, failures);
            }
            RunEvent::Region(
                _,
                RegionEvent::PassComplete {
                    pass,
                    failures,
                    elapsed,
                },
            ) => {
                sink.emit_pass_complete(pass, failures, elapsed);
                sink.print_pass_summary(pass, total_passes, failures);
            }
            RunEvent::Region(_, RegionEvent::EccDeltas { pass, ref deltas }) => {
                sink.emit_ecc_deltas(pass, deltas);
                sink.print_ecc_deltas(pass, deltas);
            }
            RunEvent::RunComplete => break,
            RunEvent::DimmInfo { .. } | RunEvent::Log { .. } => {}
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

    // Consumer thread drives OutputSink from events
    let consumer = std::thread::spawn(move || {
        consume_headless_events(&rx, &mut sink);
        sink
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

    let mut sink = consumer.join().expect("event consumer thread panicked");

    let config = ferrite::runner::RunConfig {
        size: cli.size,
        passes: cli.passes,
        patterns: patterns.to_vec(),
        regions: 1,
        parallel: !cli.sequential,
    };
    let mut results = ferrite::runner::RunResults::from_passes(pass_results, config, run_elapsed);

    ferrite::error_analysis::analyze(&mut results);

    if let Some(ref ea) = results.error_analysis {
        let class_str = match &ea.classification {
            ferrite::error_analysis::ErrorClassification::StuckBit { positions } => {
                let pos_str: Vec<String> = positions.iter().map(|p| format!("bit {p}")).collect();
                format!("stuck bit(s): {}", pos_str.join(", "))
            }
            ferrite::error_analysis::ErrorClassification::Coupling => {
                "coupling/disturbance errors".to_owned()
            }
            ferrite::error_analysis::ErrorClassification::Mixed => {
                "mixed (stuck + coupling)".to_owned()
            }
            ferrite::error_analysis::ErrorClassification::NoErrors => unreachable!(),
        };

        eprintln!("  Error analysis: {class_str}");
        eprintln!("  Affected bits: 0x{:016x}", ea.union_xor_mask);
        if let (Some(lo), Some(hi)) = (ea.lowest_phys, ea.highest_phys) {
            eprintln!("  Physical address range: 0x{lo:x} -- 0x{hi:x}");
        }
    }

    sink.emit_summary(cli.passes, results.total_failures, run_elapsed);
    sink.print_final_result(results.total_failures);

    let code = shutdown::exit_code(results.total_failures);
    if code != 0 {
        std::process::exit(code);
    }

    Ok(())
}
