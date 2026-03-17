use anyhow::{Context, Result};
use clap::Parser;
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

fn main() -> Result<()> {
    let cli = Cli::parse();

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
