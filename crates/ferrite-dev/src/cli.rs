//! The command line.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

use crate::bench::Phase;

#[derive(Debug, Parser)]
#[command(name = "ferrite-dev", about = "Desktop tool for the ferrite DbC link and the remote target loop")]
pub struct Cli {
    /// Config file; defaults to ~/.config/ferrite-dev/config.toml, and a missing default is fine
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print timestamped lines from the DbC link, reconnecting and re-enabling the USB port
    Log(LogArgs),
    /// Be the desktop peer of dbcbench and print desktop and target numbers side by side
    Bench(BenchArgs),
    /// Build an app, boot it once on the target, capture its output and collect its reports
    Run(RunArgs),
    /// Print the effective configuration
    Config,
}

#[derive(Debug, Args)]
pub struct LogArgs {
    /// Stop after this long (like 90s or 5m); without it, capture until interrupted
    #[arg(long = "for", value_name = "DURATION", value_parser = parse_duration)]
    pub duration: Option<Duration>,

    /// Also write the lines to this file
    #[arg(long, value_name = "FILE")]
    pub out: Option<PathBuf>,

    /// Send this line to the target about 4 s after each connect
    #[arg(long, value_name = "TEXT")]
    pub probe: Option<String>,
}

#[derive(Debug, Args)]
pub struct BenchArgs {
    /// Phases to exercise and report; the others are followed passively so the target advances
    #[arg(long, value_delimiter = ',', default_values_t = Phase::ALL)]
    pub phases: Vec<Phase>,

    /// How long to wait for the device and the target's first announcement
    #[arg(long, value_name = "DURATION", default_value = "5m", value_parser = parse_duration)]
    pub wait: Duration,

    /// Also write the output to this file
    #[arg(long, value_name = "FILE")]
    pub out: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// The UEFI binary to run, like dbcping or dbcbench
    pub app: String,

    /// Longest a log capture runs; the capture also stops when the target is back
    #[arg(long, value_name = "DURATION", default_value = "5m", value_parser = parse_duration)]
    pub log_for: Duration,

    /// Skip the DbC capture
    #[arg(long)]
    pub no_capture: bool,

    /// How long the target gets to go down and come back
    #[arg(long, value_name = "DURATION", default_value = "300s", value_parser = parse_duration)]
    pub timeout: Duration,
}

/// A duration like `90s` or `5m`; a bare number is seconds.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    text.parse::<u64>().map_or_else(
        |_| humantime::parse_duration(text).map_err(|e| e.to_string()),
        |secs| Ok(Duration::from_secs(secs)),
    )
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};
    use clap::CommandFactory;
    use rstest::rstest;

    use super::*;

    #[test]
    fn the_command_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[rstest]
    #[case::bare_seconds("150", Ok(Duration::from_secs(150)))]
    #[case::unit("90s", Ok(Duration::from_secs(90)))]
    #[case::minutes("5m", Ok(Duration::from_secs(300)))]
    #[case::compound("1m 30s", Ok(Duration::from_secs(90)))]
    #[case::garbage("soon", Err(()))]
    fn durations_parse(#[case] text: &str, #[case] want: Result<Duration, ()>) {
        check!(parse_duration(text).map_err(|_| ()) == want);
    }

    #[test]
    fn bench_phases_default_to_all_and_split_on_commas() {
        assert!(let Ok(cli) = Cli::try_parse_from(["ferrite-dev", "bench"]));
        assert!(let Command::Bench(args) = cli.command);
        check!(args.phases == Phase::ALL);
        assert!(let Ok(cli) = Cli::try_parse_from(["ferrite-dev", "bench", "--phases", "echo,micro", "--wait", "90s"]));
        assert!(let Command::Bench(args) = cli.command);
        check!(args.phases == [Phase::Echo, Phase::Micro]);
        check!(args.wait == Duration::from_secs(90));
    }

    #[test]
    fn run_takes_an_app_and_the_documented_flags() {
        assert!(let Ok(cli) = Cli::try_parse_from(["ferrite-dev", "run", "dbcping", "--log-for", "2m", "--no-capture"]));
        assert!(let Command::Run(args) = cli.command);
        check!((args.app.as_str(), args.log_for, args.no_capture) == ("dbcping", Duration::from_secs(120), true));
        check!(args.timeout == Duration::from_secs(300));
    }
}
