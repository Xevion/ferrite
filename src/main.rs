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
use cli::{Cli, OutputConfig, OutputFormat, SetupOutcome, check_privileges, setup_test};

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

    // /dev/mem targeted testing is a distinct, always-headless backend: it maps
    // chosen physical ranges rather than anonymous memory.
    if let Some(target) = cli.devmem {
        drop(tracing_handle);
        let result = run_devmem(&cli, target, &patterns, workers, parallel);
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
                anyhow::bail!(
                    "--format json is not supported with TUI mode. \
                     Use --tui never for JSON output."
                );
            }

            let events_writer = open_events_writer(&output)?;

            let cull = cull_ranges(&cli, coverage_ctx.as_ref());
            let s = match setup_test(&cli, cull.as_deref())? {
                SetupOutcome::Ready(s) => s,
                SetupOutcome::CullCeiling => {
                    report_cull_ceiling(
                        coverage_ctx.as_ref(),
                        cull.as_deref().unwrap_or(&[]),
                        &output,
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

/// Report the `--cull`-at-ceiling outcome: every acquirable frame is already
/// covered, so no run happened and the process exits successfully. Renders
/// cumulative coverage plus the gap classification for table output; JSON
/// output stays empty (no run events occurred) with the detail on stderr.
fn report_cull_ceiling(
    ctx: Option<&CoverageCtx>,
    covered: &[ferrite::coverage::PfnRange],
    output: &OutputConfig,
    unit_system: ferrite::units::UnitSystem,
) {
    tracing::info!(
        "--cull: nothing new to test; every acquirable frame is already covered on this boot"
    );
    if output.format != OutputFormat::Table {
        return;
    }
    let gap = ferrite::gap::classify_system_gaps(covered);
    let installed = ferrite::sysmem::installed_ram().map_or(0, |r| r.bytes);
    let (cumulative, runs) = ctx.map_or((0, 0), |c| {
        (c.store.covered_bytes(), c.store.runs.len() as u64)
    });
    ferrite::results::render_ceiling_report(
        &mut std::io::stdout(),
        cumulative,
        installed,
        runs,
        gap,
        unit_system,
    )
    .unwrap_or_else(|e| eprintln!("warning: failed to render results: {e}"));
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

/// `/dev/mem` targeted testing: resolve the requested target into concrete
/// physical mappings, then test (or read-only probe) each in turn. Always
/// headless. Exits with a non-zero code if any mapping's write test fails.
fn run_devmem(
    cli: &Cli,
    target: ferrite::devmem::DevMemTarget,
    patterns: &[Pattern],
    workers: usize,
    parallel: bool,
) -> Result<()> {
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let iomem = std::fs::read_to_string("/proc/iomem").unwrap_or_default();
    let system_ram = ferrite::sysmem::system_ram_ranges(&iomem);

    let mappings = ferrite::devmem::resolve_mappings(target, &cmdline, &system_ram)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let mut total_failures: usize = 0;
    for mapping in mappings {
        total_failures += run_devmem_mapping(&mapping, cli, patterns, workers, parallel)?;
    }

    let code = shutdown::exit_code(total_failures);
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// Test or probe a single physical mapping according to its safety class and
/// the `--devmem-unsafe` override. Returns the number of failures found.
fn run_devmem_mapping(
    mapping: &ferrite::devmem::Mapping,
    cli: &Cli,
    patterns: &[Pattern],
    workers: usize,
    parallel: bool,
) -> Result<usize> {
    use ferrite::devmem::{Safety, write_allowed};

    let start = mapping.phys_start;
    let end = mapping.phys_start + mapping.len as u64 - 1;
    let label = match mapping.safety {
        Safety::Reserved => "reserved",
        Safety::SystemRam => "System RAM",
        Safety::FirmwareOrMmio => "firmware/MMIO",
    };

    if matches!(mapping.safety, Safety::FirmwareOrMmio) {
        tracing::warn!("devmem: refusing {start:#x}-{end:#x} ({label}) -- never safe to touch");
        return Ok(0);
    }

    if write_allowed(mapping.safety, cli.devmem_unsafe) {
        if matches!(mapping.safety, Safety::SystemRam) {
            tracing::warn!(
                "devmem: --devmem-unsafe write-testing LIVE System RAM {start:#x}-{end:#x} -- \
                 this can corrupt the kernel and crash the machine"
            );
        }
        tracing::info!("devmem: write-testing physical {start:#x}-{end:#x} ({label})");
        run_devmem_write(mapping, cli, patterns, workers, parallel)
    } else {
        tracing::info!(
            "devmem: read-only probe of physical {start:#x}-{end:#x} ({label}); \
             pass --devmem-unsafe to write-test (DANGEROUS)"
        );
        run_devmem_probe(mapping, cli.units)?;
        Ok(0)
    }
}

/// Context for a `/dev/mem` mapping failure. Live System RAM cannot be mmap'd
/// while it sits in the kernel's direct map (a PAT memtype conflict yields
/// EINVAL), so point the user at the ways to remove it from the direct map.
fn devmem_map_context(mapping: &ferrite::devmem::Mapping) -> String {
    if matches!(mapping.safety, ferrite::devmem::Safety::SystemRam) {
        "failed to map /dev/mem: the kernel blocks mapping live System RAM that is already \
         in its direct map. Fence the range with memmap= at boot, or offline its memory \
         block, then retest through /dev/mem"
            .to_owned()
    } else {
        "failed to map physical range through /dev/mem".to_owned()
    }
}

/// Run the pattern suite against a writable `/dev/mem` mapping, streaming live
/// output through the headless printer. Physical addresses of failures are
/// resolved exactly (no pagemap) via [`ferrite::devmem::DevMemResolver`].
fn run_devmem_write(
    mapping: &ferrite::devmem::Mapping,
    cli: &Cli,
    patterns: &[Pattern],
    workers: usize,
    parallel: bool,
) -> Result<usize> {
    let mut buf = ferrite::alloc::TestBuffer::map_physical(mapping.phys_start, mapping.len, true)
        .with_context(|| devmem_map_context(mapping))?;
    let mut resolver =
        ferrite::devmem::DevMemResolver::new(buf.as_ptr(), mapping.phys_start, mapping.len);
    let map_stats = resolver.build_map(buf.as_ptr(), mapping.len).ok();

    let unit_system = cli.units;
    let (tx, rx) = ferrite::events::event_bus();
    let consumer = std::thread::spawn(move || {
        let mut printer = HeadlessPrinter::new(std::io::stdout(), unit_system);
        printer.consume(&rx);
        printer
    });

    let _ = tx.send(RunEvent::RunStart {
        size: mapping.len,
        passes: cli.passes,
        patterns: patterns.to_vec(),
        workers,
    });
    if let Some(stats) = map_stats {
        let _ = tx.send(RunEvent::MapInfo { stats });
    }

    let run_start = std::time::Instant::now();
    let pass_results = runner::run(
        buf.as_u64_slice_mut(),
        patterns,
        cli.passes,
        parallel,
        &tx,
        Some(&resolver as &(dyn PhysResolver + Sync)),
        &|_| {},
    )
    .context("pattern execution failed")?;
    let elapsed = run_start.elapsed();

    let _ = tx.send(RunEvent::RunComplete);
    drop(tx);
    let mut printer = consumer.join().expect("event consumer thread panicked");

    let config = ferrite::runner::RunConfig {
        size: mapping.len,
        passes: cli.passes,
        patterns: patterns.to_vec(),
        workers,
    };
    let results = ferrite::runner::RunResults::from_passes(pass_results, config, elapsed);
    printer.print_final_result(results.total_failures);
    Ok(results.total_failures)
}

/// Read-only reachability probe of a physical range via `pread` on `/dev/mem`.
///
/// Unlike `mmap`, `pread` reads live System RAM without hitting the direct-map
/// memtype conflict, so this works where the write path cannot. It never
/// writes, so it is always safe; live RAM mutates under the read, making the
/// checksum a reachability signal rather than a stable value.
fn run_devmem_probe(
    mapping: &ferrite::devmem::Mapping,
    unit_system: ferrite::units::UnitSystem,
) -> Result<()> {
    use std::os::unix::fs::FileExt;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .open("/dev/mem")
        .context("failed to open /dev/mem (run as root)")?;

    let end = mapping.phys_start + mapping.len as u64;
    let mut offset = mapping.phys_start;
    let mut chunk = vec![0u8; 4 * 1024 * 1024];
    let mut stats = ferrite::devmem::ProbeStats::default();
    while offset < end {
        let n = ((end - offset) as usize).min(chunk.len());
        file.read_exact_at(&mut chunk[..n], offset)
            .with_context(|| format!("pread /dev/mem at {offset:#x}"))?;
        stats = stats.merge(ferrite::devmem::probe_bytes(&chunk[..n]));
        offset += n as u64;
    }

    let size = ferrite::units::Size::new(mapping.len as f64, unit_system);
    println!(
        "  probe: {size} readable ({} words, {} nonzero, checksum {:#018x})",
        stats.words_read, stats.nonzero_words, stats.xor_checksum,
    );
    Ok(())
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
    let mut setup = match setup_test(cli, cull.as_deref())? {
        SetupOutcome::Ready(s) => s,
        SetupOutcome::CullCeiling => {
            report_cull_ceiling(
                coverage_ctx.as_ref(),
                cull.as_deref().unwrap_or(&[]),
                output,
                cli.units,
            );
            return Ok(());
        }
    };
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
