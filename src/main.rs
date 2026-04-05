#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

#[cfg(feature = "tui")]
use std::io::IsTerminal;

use anyhow::{Context, Result};
use clap::Parser;

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
            // run_tui_mode calls process::exit internally
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

/// Non-TUI mode: headless output with tracing to stderr.
fn run_non_tui(cli: &Cli, patterns: &[Pattern], mut sink: OutputSink) -> Result<()> {
    #[cfg(feature = "tui")]
    setup_tracing(sink.is_json(), None);

    let mut setup = setup_test(cli)?;

    if let Some(ref stats) = setup.map_stats {
        sink.emit_map_info(stats);
        sink.print_map_info(stats);
    }

    let run_start = std::time::Instant::now();
    let results = runner::run(
        setup.region.as_u64_slice_mut(),
        patterns,
        cli.passes,
        !cli.sequential,
        &mut sink,
        setup.resolver.as_ref().map(|r| r as &dyn PhysResolver),
        &|_| {},
    )
    .context("pattern execution failed")?;
    let run_elapsed = run_start.elapsed();

    let total_failures: usize = results
        .iter()
        .map(ferrite::runner::PassResult::total_failures)
        .sum();

    if total_failures > 0 {
        let mut stats = ferrite::error_analysis::BitErrorStats::new();
        for pass_result in &results {
            for pattern_result in &pass_result.pattern_results {
                for f in &pattern_result.failures {
                    stats.record(f);
                }
            }
        }

        let classification = stats.classification();
        let class_str = match &classification {
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
        eprintln!("  Affected bits: 0x{:016x}", stats.union_xor_mask);
        if let (Some(lo), Some(hi)) = (stats.lowest_phys, stats.highest_phys) {
            eprintln!("  Physical address range: 0x{lo:x} -- 0x{hi:x}");
        }
    }

    sink.emit_summary(cli.passes, total_failures, run_elapsed);
    sink.print_final_result(total_failures);

    let code = shutdown::exit_code(total_failures);
    if code != 0 {
        std::process::exit(code);
    }

    Ok(())
}
