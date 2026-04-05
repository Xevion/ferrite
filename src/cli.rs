use std::fs;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
#[cfg(feature = "tui")]
use clap::ValueEnum;
use nix::sys::resource::{Resource, getrlimit};
use nix::unistd::geteuid;
use owo_colors::OwoColorize;
use tracing::{info, warn};

use ferrite::alloc::CompactionGuard;
use ferrite::alloc::LockedRegion;
use ferrite::dimm::DimmTopology;
use ferrite::phys::{MapStats, PagemapResolver, PhysResolver};
use ferrite::units::UnitSystem;

#[cfg(feature = "tui")]
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum TuiMode {
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
pub struct Cli {
    /// Amount of memory to test (e.g. "256M", "1G", "512K").
    /// Defaults to 64M.
    #[arg(short = 's', long, default_value = "64M", value_parser = parse_size)]
    pub size: usize,

    /// Number of test passes to run.
    #[arg(short, long, default_value_t = 1)]
    pub passes: usize,

    /// Which test patterns to run. Defaults to all.
    #[arg(short = 't', long = "test", value_enum)]
    pub patterns: Vec<ferrite::pattern::Pattern>,

    /// Run patterns sequentially on a single core instead of using all CPU cores.
    #[arg(long)]
    pub sequential: bool,

    /// Unit system for sizes and throughput: binary (KiB, MiB, GiB) or decimal (KB, MB, GB).
    #[arg(long, value_enum, default_value_t = UnitSystem::Binary)]
    pub units: UnitSystem,

    /// Emit NDJSON events for structured output.
    /// Without a path (or with '-'), writes JSON to stdout and human output to stderr.
    /// With a file path, writes JSON to that file and human output to stdout.
    #[arg(long, value_name = "PATH", num_args = 0..=1, default_missing_value = "-")]
    pub json: Option<String>,

    /// TUI mode: "auto" (default) uses the TUI when stdout is a terminal,
    /// "always" forces the TUI, "never" uses plain non-interactive output.
    #[cfg(feature = "tui")]
    #[arg(long, value_enum, default_value_t = TuiMode::Auto)]
    pub tui: TuiMode,

    /// Disable physical address resolution (skip pagemap/EDAC/SMBIOS).
    #[arg(long)]
    pub no_phys: bool,

    /// Number of memory regions to test in parallel (default: CPU core count).
    /// Each region runs all patterns independently.
    #[arg(long, default_value_t = 0)]
    pub regions: usize,
}

pub fn parse_size(s: &str) -> Result<usize, String> {
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
pub fn check_privileges(requested_bytes: usize, need_phys: bool) {
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

pub fn has_capability(cap_bit: u32) -> bool {
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

/// Set up physical address resolution, returning the resolver and map stats if successful.
pub fn setup_phys(
    region: &LockedRegion,
    need_phys: bool,
) -> (Option<PagemapResolver>, Option<MapStats>) {
    if !need_phys {
        return (None, None);
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
                (Some(r), Some(stats))
            }
            Err(e) => {
                warn!("failed to build page map: {e}");
                (None, None)
            }
        },
        Err(e) => {
            warn!("pagemap unavailable: {e}");
            (None, None)
        }
    }
}

pub struct TestSetup {
    pub region: LockedRegion,
    /// Held for its [`Drop`] side-effect — restores the compaction sysctl on teardown.
    #[allow(dead_code)]
    pub compaction_guard: Option<CompactionGuard>,
    pub resolver: Option<PagemapResolver>,
    pub map_stats: Option<MapStats>,
}

pub fn setup_test(cli: &Cli) -> Result<TestSetup> {
    let need_phys = !cli.no_phys;
    let region = LockedRegion::new(cli.size).context("failed to allocate and lock memory")?;
    let compaction_guard = if need_phys {
        CompactionGuard::new()
    } else {
        None
    };
    let (resolver, map_stats) = setup_phys(&region, need_phys);

    if need_phys && let Some(topo) = DimmTopology::build() {
        let dimm_str = topo
            .dimms
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ");
        info!("installed DIMMs: {dimm_str}");
    }

    Ok(TestSetup {
        region,
        compaction_guard,
        resolver,
        map_stats,
    })
}
