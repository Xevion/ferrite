use std::fs;
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use nix::sys::resource::{Resource, getrlimit};
use nix::unistd::geteuid;
use owo_colors::OwoColorize;
use tracing::{info, warn};

use ferrite::alloc::LockedRegion;
use ferrite::dimm::DimmTopology;
use ferrite::edac::EdacSnapshot;
use ferrite::output::OutputSink;
use ferrite::pattern::{Pattern, run_pattern};
use ferrite::phys::{PagemapResolver, PhysResolver};
use ferrite::runner;
use ferrite::stability::CompactionGuard;
use ferrite::tui::{RegionState, TuiConfig, TuiError, TuiEvent, TuiMakeWriter};
use ferrite::units::UnitSystem;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum TuiMode {
    /// Use TUI when stdout is a terminal, plain output otherwise.
    Auto,
    /// Always use the interactive TUI.
    Always,
    /// Never use the TUI; use plain non-interactive output.
    Never,
}

/// ferrite -- userspace RAM testing tool for Linux
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Amount of memory to test (e.g. "256M", "1G", "512K").
    /// Defaults to 64M.
    #[arg(short = 's', long, default_value = "64M", value_parser = parse_size)]
    size: usize,

    /// Number of test passes to run.
    #[arg(short, long, default_value_t = 1)]
    passes: usize,

    /// Which test patterns to run. Defaults to all.
    #[arg(short = 't', long = "test", value_enum)]
    patterns: Vec<Pattern>,

    /// Run patterns sequentially on a single core instead of using all CPU cores.
    #[arg(long)]
    sequential: bool,

    /// Unit system for sizes and throughput: binary (KiB, MiB, GiB) or decimal (KB, MB, GB).
    #[arg(long, value_enum, default_value_t = UnitSystem::Binary)]
    units: UnitSystem,

    /// Emit NDJSON events for structured output.
    /// Without a path (or with '-'), writes JSON to stdout and human output to stderr.
    /// With a file path, writes JSON to that file and human output to stdout.
    #[arg(long, value_name = "PATH", num_args = 0..=1, default_missing_value = "-")]
    json: Option<String>,

    /// TUI mode: "auto" (default) uses the TUI when stdout is a terminal,
    /// "always" forces the TUI, "never" uses plain non-interactive output.
    #[arg(long, value_enum, default_value_t = TuiMode::Auto)]
    tui: TuiMode,

    /// Disable physical address resolution (skip pagemap/EDAC/SMBIOS).
    #[arg(long)]
    no_phys: bool,

    /// Number of memory regions to test in parallel (default: CPU core count).
    /// Each region runs all patterns independently.
    #[arg(long, default_value_t = 0)]
    regions: usize,
}

fn parse_size(s: &str) -> Result<usize, String> {
    let s = s.trim();
    let (num_str, multiplier) = if let Some(n) = s.strip_suffix(['G', 'g']) {
        (n, 1024 * 1024 * 1024)
    } else if let Some(n) = s.strip_suffix(['M', 'm']) {
        (n, 1024 * 1024)
    } else if let Some(n) = s.strip_suffix(['K', 'k']) {
        (n, 1024)
    } else {
        (s, 1)
    };
    let num: usize = num_str.parse().map_err(|_| format!("invalid size: {s}"))?;
    Ok(num * multiplier)
}

/// Check whether the process has sufficient privileges to mlock memory.
fn check_privileges(requested_bytes: usize, need_phys: bool) {
    let is_root = geteuid().is_root();
    let has_ipc_lock = has_capability(14); // CAP_IPC_LOCK
    let has_sys_admin = has_capability(21); // CAP_SYS_ADMIN

    if need_phys && !is_root && !has_sys_admin {
        eprintln!(
            "{} CAP_SYS_ADMIN not detected -- physical addresses will be unavailable",
            "warning:".yellow().bold(),
        );
        eprintln!(
            "         run as root for physical address resolution: {}",
            "sudo ferrite".bold()
        );
    }

    if is_root || has_ipc_lock {
        return;
    }

    match getrlimit(Resource::RLIMIT_MEMLOCK) {
        Ok((soft, _hard)) => {
            if soft != u64::MAX && (requested_bytes as u64) > soft {
                eprintln!(
                    "{} RLIMIT_MEMLOCK is {} bytes, but {} bytes requested",
                    "warning:".yellow().bold(),
                    soft,
                    requested_bytes,
                );
                eprintln!("         mlock will likely fail. Options:");
                eprintln!("           - run as root: {}", "sudo ferrite".bold());
                eprintln!(
                    "           - raise the limit: {}",
                    "ulimit -l unlimited".bold()
                );
                eprintln!(
                    "           - grant the capability: {}",
                    "sudo setcap cap_ipc_lock+ep $(which ferrite)".bold()
                );
            }
        }
        Err(e) => {
            eprintln!(
                "{} could not query RLIMIT_MEMLOCK: {e}",
                "warning:".yellow().bold(),
            );
        }
    }
}

fn has_capability(cap_bit: u32) -> bool {
    let Ok(status) = fs::read_to_string("/proc/self/status") else {
        return false;
    };
    status
        .lines()
        .find_map(|line| {
            let hex = line.strip_prefix("CapEff:\t")?;
            let bits = u64::from_str_radix(hex.trim(), 16).ok()?;
            Some(bits & (1 << cap_bit) != 0)
        })
        .unwrap_or(false)
}

/// Set up physical address resolution, returning the resolver if successful.
fn setup_phys(region: &LockedRegion, need_phys: bool) -> Option<PagemapResolver> {
    if !need_phys {
        return None;
    }
    match PagemapResolver::new() {
        Ok(mut r) => match r.build_map(region.as_ptr(), region.len()) {
            Ok(stats) => {
                info!(
                    pages = stats.total_pages,
                    thp = stats.thp_pages,
                    huge = stats.huge_pages,
                    hwpoison = stats.hwpoison_pages,
                    "physical address map built"
                );

                std::thread::sleep(Duration::from_millis(100));
                match r.verify_stability(region.as_ptr(), region.len()) {
                    Ok(0) => {}
                    Ok(n) => warn!(changed = n, "pages changed physical address after locking"),
                    Err(e) => warn!("PFN stability check failed: {e}"),
                }
                Some(r)
            }
            Err(e) => {
                warn!("failed to build page map: {e}");
                None
            }
        },
        Err(e) => {
            warn!("pagemap unavailable: {e}");
            None
        }
    }
}

fn main() -> Result<()> {
    let mut cli = Cli::parse();
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

    let use_tui = match cli.tui {
        TuiMode::Always => true,
        TuiMode::Never => false,
        TuiMode::Auto => std::io::stdout().is_terminal(),
    };

    if use_tui {
        run_tui_mode(cli, patterns, sink)
    } else {
        run_non_tui(cli, patterns, sink)
    }
}

/// TUI mode: the default interactive experience.
fn run_tui_mode(cli: Cli, patterns: Vec<Pattern>, _sink: OutputSink) -> Result<()> {
    let need_phys = !cli.no_phys;

    let (tx, rx) = mpsc::sync_channel::<TuiEvent>(256);
    let quit = Arc::new(AtomicBool::new(false));

    // Set up tracing with fmt formatting routed through the TUI channel.
    // When the TUI exits (rx dropped), the writer falls back to stderr.
    let writer = TuiMakeWriter::new(tx.clone());
    let subscriber = tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(true)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("failed to set tracing subscriber");

    let mut region = LockedRegion::new(cli.size).context("failed to allocate and lock memory")?;

    let _compaction_guard = if need_phys {
        CompactionGuard::new()
    } else {
        None
    };

    let resolver = setup_phys(&region, need_phys);

    // DIMM topology (best-effort)
    if need_phys && let Some(topo) = DimmTopology::build() {
        let dimm_str = topo
            .dimms
            .iter()
            .map(|d| d.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        info!("installed DIMMs: {dimm_str}");
    }

    // Compute region count
    let total_words = region.as_u64_slice().len();
    let min_words_per_region = 1024 * 1024; // 8 MiB minimum per region
    let n_regions = if cli.regions > 0 {
        cli.regions
    } else {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    }
    .min(total_words / min_words_per_region)
    .max(1);

    let chunk_words = total_words / n_regions;
    info!(
        regions = n_regions,
        "testing {} across {} region(s) with {} pattern(s)",
        ferrite::units::Size::new(cli.size as f64, ferrite::units::UnitSystem::Binary),
        n_regions,
        patterns.len()
    );

    let pattern_names: Vec<String> = patterns.iter().map(|p| p.to_string()).collect();
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
    let worker_quit = Arc::clone(&quit);
    let parallel = !cli.sequential;
    let passes = cli.passes;

    let worker = thread::spawn(move || {
        let buf = region.as_u64_slice_mut();

        thread::scope(|s| {
            let chunks: Vec<&mut [u64]> = buf.chunks_mut(chunk_words).collect();
            for (i, chunk) in chunks.into_iter().enumerate() {
                let tui_region = Arc::clone(&worker_regions[i]);
                let tx = worker_tx.clone();
                let quit = Arc::clone(&worker_quit);
                let resolver_ref = resolver.as_ref().map(|r| r as &(dyn PhysResolver + Sync));
                let patterns = &patterns;
                s.spawn(move || {
                    run_region_worker(
                        chunk,
                        patterns,
                        passes,
                        parallel,
                        i,
                        &tui_region,
                        &tx,
                        resolver_ref,
                        &quit,
                    );
                });
            }
        });
    });

    let config = TuiConfig::default();
    ferrite::tui::run_tui(&config, &regions, tx, rx, &quit).context("TUI failed")?;

    let _ = worker.join();

    let total_errors: usize = regions
        .iter()
        .map(|r| r.error_count.load(Ordering::Relaxed))
        .sum();
    if total_errors > 0 {
        std::process::exit(1);
    }

    Ok(())
}

/// Worker for a single memory region: runs test patterns and feeds results to the TUI.
#[allow(clippy::too_many_arguments)]
fn run_region_worker(
    buf: &mut [u64],
    patterns: &[Pattern],
    passes: usize,
    parallel: bool,
    region_idx: usize,
    tui_state: &Arc<RegionState>,
    tx: &mpsc::SyncSender<TuiEvent>,
    resolver: Option<&(dyn PhysResolver + Sync)>,
    quit: &Arc<AtomicBool>,
) {
    let region_bytes = buf.len() * 8;
    info!(
        region = tui_state.name.as_str(),
        "testing {} across {} pass(es) with {} pattern(s)",
        ferrite::units::Size::new(region_bytes as f64, ferrite::units::UnitSystem::Binary),
        passes,
        patterns.len()
    );

    for pass in 0..passes {
        if quit.load(Ordering::Relaxed) {
            break;
        }

        let edac_before = EdacSnapshot::capture();

        for (pat_idx, &pattern) in patterns.iter().enumerate() {
            if quit.load(Ordering::Relaxed) {
                break;
            }

            tui_state.set_pattern(pat_idx);
            info!(region = tui_state.name.as_str(), pattern = %pattern, pass = pass + 1, "starting pattern");

            while tui_state.paused.load(Ordering::Relaxed) && !quit.load(Ordering::Relaxed) {
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

/// Non-TUI mode: headless output with tracing to stderr.
fn run_non_tui(cli: Cli, patterns: Vec<Pattern>, mut sink: OutputSink) -> Result<()> {
    let subscriber = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("failed to set tracing subscriber");

    let need_phys = !cli.no_phys;

    let mut region = LockedRegion::new(cli.size).context("failed to allocate and lock memory")?;

    let _compaction_guard = if need_phys {
        CompactionGuard::new()
    } else {
        None
    };

    let resolver = setup_phys(&region, need_phys);

    if need_phys && let Some(topo) = DimmTopology::build() {
        let dimm_str = topo
            .dimms
            .iter()
            .map(|d| d.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        info!("installed DIMMs: {dimm_str}");
    }

    let run_start = Instant::now();
    let results = runner::run(
        &mut region,
        &patterns,
        cli.passes,
        !cli.sequential,
        &mut sink,
        resolver.as_ref().map(|r| r as &dyn PhysResolver),
        &|_| {},
    );
    let run_elapsed = run_start.elapsed();

    let total_failures: usize = results.iter().map(|r| r.total_failures()).sum();

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

    if total_failures > 0 {
        std::process::exit(1);
    }

    Ok(())
}
