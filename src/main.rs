use std::fs;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use nix::sys::resource::{Resource, getrlimit};
use nix::unistd::geteuid;
use owo_colors::OwoColorize;

use ferrite::alloc::LockedRegion;
use ferrite::dimm::DimmTopology;
use ferrite::error_analysis::BitErrorStats;
use ferrite::output::OutputSink;
use ferrite::pattern::Pattern;
use ferrite::phys::{PagemapResolver, PhysResolver};
use ferrite::runner;
use ferrite::stability::CompactionGuard;
use ferrite::units::UnitSystem;

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

    /// Emit NDJSON events instead of human-readable output.
    /// Without a path (or with '-'), writes JSON to stdout and human output to stderr.
    /// With a file path, writes JSON to that file and human output to stdout.
    #[arg(long, value_name = "PATH", num_args = 0..=1, default_missing_value = "-")]
    json: Option<String>,

    /// Disable physical address resolution (skip pagemap/EDAC/SMBIOS).
    #[arg(long)]
    no_phys: bool,
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
/// Prints warnings if issues are detected but does not exit.
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

    // Root and CAP_IPC_LOCK both bypass RLIMIT_MEMLOCK entirely.
    if is_root || has_ipc_lock {
        return;
    }

    // Without root or CAP_IPC_LOCK, mlock is governed by RLIMIT_MEMLOCK.
    // Only warn if the limit is too small for the requested allocation.
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

/// Check if the current process has a given capability in its effective set.
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

fn main() -> Result<()> {
    let cli = Cli::parse();
    let need_phys = !cli.no_phys;
    check_privileges(cli.size, need_phys);

    let patterns = if cli.patterns.is_empty() {
        Pattern::ALL.to_vec()
    } else {
        cli.patterns
    };

    let mut sink = match &cli.json {
        None => OutputSink::human(cli.units),
        Some(path) => OutputSink::json(path, cli.units).context("failed to open JSON output")?,
    };

    let mut region = LockedRegion::new(cli.size).context("failed to allocate and lock memory")?;

    // Physical address resolution setup
    let _compaction_guard = if need_phys {
        CompactionGuard::new()
    } else {
        None
    };

    let resolver = if need_phys {
        match PagemapResolver::new() {
            Ok(mut r) => {
                match r.build_map(region.as_ptr(), region.len()) {
                    Ok(stats) => {
                        sink.emit_map_info(&stats);
                        sink.print_map_info(&stats);

                        // Brief pause then verify PFN stability
                        std::thread::sleep(Duration::from_millis(100));
                        match r.verify_stability(region.as_ptr(), region.len()) {
                            Ok(0) => {}
                            Ok(n) => {
                                eprintln!(
                                    "{} {n} pages changed physical address after locking -- physical addresses may be inaccurate",
                                    "warning:".yellow().bold(),
                                );
                            }
                            Err(e) => {
                                eprintln!(
                                    "{} PFN stability check failed: {e}",
                                    "warning:".yellow().bold(),
                                );
                            }
                        }
                        Some(r)
                    }
                    Err(e) => {
                        eprintln!(
                            "{} failed to build page map: {e}",
                            "warning:".yellow().bold(),
                        );
                        None
                    }
                }
            }
            Err(e) => {
                eprintln!("{} pagemap unavailable: {e}", "warning:".yellow().bold(),);
                None
            }
        }
    } else {
        None
    };

    // DIMM topology (best-effort)
    let _dimm_topo = if need_phys {
        let topo = DimmTopology::build();
        if let Some(ref topo) = topo {
            let line = format!(
                "  Installed DIMMs: {}",
                topo.dimms
                    .iter()
                    .map(|d| d.to_string())
                    .collect::<Vec<_>>()
                    .join("; ")
            );
            if sink.is_json() {
                eprintln!("{line}");
            } else {
                println!("{line}");
            }
        }
        topo
    } else {
        None
    };
    let run_start = Instant::now();
    let results = runner::run(
        &mut region,
        &patterns,
        cli.passes,
        !cli.sequential,
        &mut sink,
        resolver.as_ref().map(|r| r as &dyn PhysResolver),
    );
    let run_elapsed = run_start.elapsed();

    let total_failures: usize = results.iter().map(|r| r.total_failures()).sum();

    // Aggregate bit error statistics if there were failures
    if total_failures > 0 {
        let mut stats = BitErrorStats::new();
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

        let mut summary_lines = vec![format!("  Error analysis: {class_str}")];
        summary_lines.push(format!("  Affected bits: 0x{:016x}", stats.union_xor_mask));
        if let (Some(lo), Some(hi)) = (stats.lowest_phys, stats.highest_phys) {
            summary_lines.push(format!("  Physical address range: 0x{lo:x} — 0x{hi:x}"));
        }

        for line in &summary_lines {
            if sink.is_json() {
                eprintln!("{line}");
            } else {
                println!("{line}");
            }
        }
    }

    sink.emit_summary(cli.passes, total_failures, run_elapsed);
    sink.print_final_result(total_failures);

    if total_failures > 0 {
        std::process::exit(1);
    }

    Ok(())
}
