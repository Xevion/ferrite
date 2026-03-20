use std::fs;

use anyhow::{Context, Result};
use clap::Parser;
use nix::sys::resource::{Resource, getrlimit};
use nix::unistd::geteuid;
use owo_colors::OwoColorize;

use ferrite::alloc::LockedRegion;
use ferrite::pattern::Pattern;
use ferrite::runner;

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
fn check_privileges(requested_bytes: usize) {
    let is_root = geteuid().is_root();
    let has_cap = has_cap_ipc_lock();

    // Root and CAP_IPC_LOCK both bypass RLIMIT_MEMLOCK entirely.
    if is_root || has_cap {
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

/// Check if the current process has CAP_IPC_LOCK (bit 14) in its effective set.
fn has_cap_ipc_lock() -> bool {
    const CAP_IPC_LOCK: u32 = 14;
    let Ok(status) = fs::read_to_string("/proc/self/status") else {
        return false;
    };
    status
        .lines()
        .find_map(|line| {
            let hex = line.strip_prefix("CapEff:\t")?;
            let bits = u64::from_str_radix(hex.trim(), 16).ok()?;
            Some(bits & (1 << CAP_IPC_LOCK) != 0)
        })
        .unwrap_or(false)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    check_privileges(cli.size);

    let patterns = if cli.patterns.is_empty() {
        Pattern::ALL.to_vec()
    } else {
        cli.patterns
    };

    let mut region = LockedRegion::new(cli.size).context("failed to allocate and lock memory")?;

    let results = runner::run(&mut region, &patterns, cli.passes, !cli.sequential);

    let total_failures: usize = results.iter().map(|r| r.total_failures()).sum();
    if total_failures == 0 {
        println!("{}", "All tests passed.".green().bold());
    } else {
        println!(
            "{}",
            format!("{total_failures} failure(s) detected.")
                .red()
                .bold(),
        );
        std::process::exit(1);
    }

    Ok(())
}
