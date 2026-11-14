//! The command line.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

use crate::bench::Phase;
use crate::link::{
    DEFAULT_IN_SIZE, DEFAULT_IN_TRANSFERS, DEFAULT_OUT_SIZE, DEFAULT_OUT_TRANSFERS, LinkOptions, Transport, UsbConfig,
};
use crate::loader::LoaderAction;

const MAX_TRANSFERS: usize = 1024;
const MAX_TRANSFER_SIZE: usize = 16 << 20;

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
    /// Build an app for its target into a .efi, and print its path or copy it to --out
    Build(BuildArgs),
    /// Build an app, boot it once on the target, capture its output and collect its reports
    Run(RunArgs),
    /// Send a .efi to the resident loader, run it there and stream its output
    Push(PushArgs),
    /// Start the resident loader on the target, or have it reset the target
    Loader(LoaderArgs),
    /// Fetch the pinned Linux release and regenerate the untracked snapshot ferrite-wifi-sys compiles
    Vendor(VendorArgs),
    /// Regenerate the stubs for the kernel symbols the Linux C imports and nothing defines
    Stubs(StubsArgs),
    /// Copy the firmware files the compiled Wi-Fi drivers can request from the host into the stick's firmware directory
    Firmware(FirmwareArgs),
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

    #[command(flatten)]
    pub link: LinkArgs,
}

#[derive(Debug, Args)]
pub struct LinkArgs {
    /// How to reach the target: usb drives the device directly, tty uses the `usb_debug` tty
    #[arg(long, value_enum, default_value_t = Transport::Usb)]
    pub transport: Transport,

    /// USB transport: IN transfers kept in flight
    #[arg(long, value_name = "N", default_value_t = DEFAULT_IN_TRANSFERS, value_parser = parse_count)]
    pub usb_in_transfers: usize,

    /// USB transport: size of each IN transfer, like 16KiB
    #[arg(long, value_name = "SIZE", default_value_t = DEFAULT_IN_SIZE, value_parser = parse_size)]
    pub usb_in_size: usize,

    /// USB transport: OUT transfers kept in flight; 8 x 64 KiB was enough on the target, 32 measured the same
    #[arg(long, value_name = "N", default_value_t = DEFAULT_OUT_TRANSFERS, value_parser = parse_count)]
    pub usb_out_transfers: usize,

    /// USB transport: size of each OUT transfer, like 64KiB
    #[arg(long, value_name = "SIZE", default_value_t = DEFAULT_OUT_SIZE, value_parser = parse_size)]
    pub usb_out_size: usize,
}

impl LinkArgs {
    pub const fn options(&self) -> LinkOptions {
        LinkOptions {
            transport: self.transport,
            usb: UsbConfig {
                in_transfers: self.usb_in_transfers,
                in_size: self.usb_in_size,
                out_transfers: self.usb_out_transfers,
                out_size: self.usb_out_size,
            },
        }
    }
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

    #[command(flatten)]
    pub link: LinkArgs,
}

#[derive(Debug, Args)]
pub struct PushArgs {
    /// The binary to build and push (like dbcping), or the path of a .efi file
    pub app: String,

    /// Replace the running loader with this image instead of running it as an app
    #[arg(long)]
    pub loader: bool,

    /// Longest the app may run before the firmware resets the target; the default is 300s
    #[arg(long, value_name = "DURATION", value_parser = parse_duration)]
    pub watchdog: Option<Duration>,

    /// How long to wait for the loader to answer
    #[arg(long, value_name = "DURATION", default_value = "10s", value_parser = parse_duration)]
    pub wait: Duration,

    /// Start the loader on the target, as `loader boot` does, when none answers within --wait
    #[arg(long)]
    pub boot: bool,

    /// With --boot: how long the loader gets to come up after the target is told to reboot into it
    #[arg(long, value_name = "DURATION", default_value = "3m", value_parser = parse_duration, requires = "boot")]
    pub boot_timeout: Duration,

    /// Reset the target into its default OS once the app has exited, and confirm the DbC device leaves
    #[arg(long, conflicts_with = "loader")]
    pub reboot: bool,

    /// Ask the app to stop once it has run this long (like 90s or 5m); Ctrl-C asks too, and a second leaves it running
    #[arg(long, value_name = "DURATION", value_parser = parse_duration, conflicts_with = "loader")]
    pub stop_after: Option<Duration>,

    /// Also write the app's log lines to this file
    #[arg(long, value_name = "FILE")]
    pub out: Option<PathBuf>,

    #[command(flatten)]
    pub link: LinkArgs,
}

#[derive(Debug, Args)]
pub struct LoaderArgs {
    pub action: LoaderAction,

    /// How long to wait: for `boot` the loader to come up (default 3m), for `reboot` the loader to answer (default 15s)
    #[arg(long, value_name = "DURATION", value_parser = parse_duration)]
    pub timeout: Option<Duration>,

    #[command(flatten)]
    pub link: LinkArgs,
}

#[derive(Debug, Args)]
pub struct VendorArgs {
    /// Regenerate the snapshot even when its inputs are unchanged
    #[arg(long)]
    pub force: bool,

    /// The ferrite-wifi-sys crate directory, which holds vendor.toml and receives linux/
    #[arg(long, value_name = "DIR", default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/../ferrite-wifi-sys"))]
    pub crate_dir: PathBuf,
}

#[derive(Debug, Args)]
pub struct BuildArgs {
    /// The binary to build, like ferrite-uefi or wifitrace
    pub app: String,

    /// Copy the .efi here, such as an ESP's bootx64.efi
    #[arg(long, value_name = "FILE")]
    pub out: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct StubsArgs {
    /// Fail if a generated file would change, and write nothing
    #[arg(long)]
    pub check: bool,

    /// The ferrite-wifi-sys crate directory, which holds the Linux C inputs and receives glue/
    #[arg(long, value_name = "DIR", default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/../ferrite-wifi-sys"))]
    pub wifi_dir: PathBuf,

    /// The ferrite-lkpi crate directory, which receives the Rust stubs
    #[arg(long, value_name = "DIR", default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/../ferrite-lkpi"))]
    pub lkpi_dir: PathBuf,
}

#[derive(Debug, Args)]
pub struct FirmwareArgs {
    /// The host's firmware directory
    #[arg(long, value_name = "DIR", default_value = "/lib/firmware")]
    pub source: PathBuf,

    /// Where the files go, as `\\EFI\\ferrite\\firmware\\` on the stick
    #[arg(long, value_name = "DIR", default_value = "esp/efi/ferrite/firmware")]
    pub dest: PathBuf,

    /// The chips to stage firmware for: name patterns, one per line
    #[arg(long, value_name = "FILE", default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/../ferrite-wifi-sys/firmware-chips.txt"))]
    pub chips: PathBuf,

    /// The workspace root, where the Linux archive is built
    #[arg(long, value_name = "DIR", default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))]
    pub root: PathBuf,

    /// Stage every file the drivers can request, not only the listed chips'
    #[arg(long)]
    pub all: bool,

    /// Report what would be staged and copy nothing
    #[arg(long)]
    pub list: bool,
}

/// A transfer count between 1 and 1024.
pub fn parse_count(text: &str) -> Result<usize, String> {
    let count: usize = text.parse().map_err(|e| format!("{e}"))?;
    if (1..=MAX_TRANSFERS).contains(&count) { Ok(count) } else { Err(format!("must be between 1 and {MAX_TRANSFERS}")) }
}

/// A byte count like `65536`, `64k`, `64KiB` or `1MiB`, between 1 byte and 16 MiB.
pub fn parse_size(text: &str) -> Result<usize, String> {
    let lower = text.trim().to_ascii_lowercase();
    let digits = lower.trim_end_matches(|c: char| !c.is_ascii_digit());
    let multiplier = match &lower[digits.len()..] {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        other => return Err(format!("unknown unit `{other}`, use KiB or MiB")),
    };
    let bytes = digits
        .parse::<usize>()
        .map_err(|e| format!("{e}"))?
        .checked_mul(multiplier)
        .ok_or_else(|| "too large".to_owned())?;
    if (1..=MAX_TRANSFER_SIZE).contains(&bytes) {
        Ok(bytes)
    } else {
        Err("must be between 1 byte and 16 MiB".to_owned())
    }
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

    #[rstest]
    #[case::bytes("65536", Ok(65536))]
    #[case::kib("64KiB", Ok(64 << 10))]
    #[case::short_k("16k", Ok(16 << 10))]
    #[case::mib("1MiB", Ok(1 << 20))]
    #[case::zero("0", Err(()))]
    #[case::too_big("17MiB", Err(()))]
    #[case::unit("5GiB", Err(()))]
    #[case::garbage("big", Err(()))]
    fn sizes_parse(#[case] text: &str, #[case] want: Result<usize, ()>) {
        check!(parse_size(text).map_err(|_| ()) == want);
    }

    #[test]
    fn the_transport_defaults_to_usb_and_takes_transfer_overrides() {
        assert!(let Ok(cli) = Cli::try_parse_from(["ferrite-dev", "bench"]));
        assert!(let Command::Bench(args) = cli.command);
        check!(args.link.options() == LinkOptions::default());
        check!(args.link.options().usb.to_string() == "16 x 16 KiB in, 8 x 64 KiB out");

        let line = [
            "ferrite-dev",
            "run",
            "dbcbench",
            "--transport",
            "tty",
            "--usb-in-transfers",
            "4",
            "--usb-out-size",
            "128k",
        ];
        assert!(let Ok(cli) = Cli::try_parse_from(line));
        assert!(let Command::Run(args) = cli.command);
        let options = args.link.options();
        check!(options.transport == Transport::Tty);
        check!((options.usb.in_transfers, options.usb.out_size) == (4, 128 << 10));
        check!(Cli::try_parse_from(["ferrite-dev", "bench", "--usb-out-transfers", "0"]).is_err());
    }

    #[test]
    fn push_takes_an_app_and_loader_flags_with_their_defaults() {
        assert!(let Ok(cli) = Cli::try_parse_from(["ferrite-dev", "push", "dbcping"]));
        assert!(let Command::Push(args) = cli.command);
        check!((args.app.as_str(), args.loader, args.boot, args.reboot) == ("dbcping", false, false, false));
        check!((args.watchdog, args.wait) == (None, Duration::from_secs(10)));

        let line = ["ferrite-dev", "push", "a.efi", "--boot", "--boot-timeout", "5m", "--watchdog", "90s", "--reboot"];
        assert!(let Ok(cli) = Cli::try_parse_from(line));
        assert!(let Command::Push(args) = cli.command);
        check!((args.boot_timeout, args.watchdog) == (Duration::from_secs(300), Some(Duration::from_secs(90))));

        check!(Cli::try_parse_from(["ferrite-dev", "push", "a.efi", "--boot-timeout", "5m"]).is_err());
        check!(Cli::try_parse_from(["ferrite-dev", "push", "a.efi", "--loader", "--reboot"]).is_err());

        assert!(let Ok(cli) = Cli::try_parse_from(["ferrite-dev", "push", "ferrite-uefi", "--stop-after", "90s"]));
        assert!(let Command::Push(args) = cli.command);
        check!(args.stop_after == Some(Duration::from_secs(90)));
        check!(Cli::try_parse_from(["ferrite-dev", "push", "a.efi", "--loader", "--stop-after", "5s"]).is_err());
    }

    #[test]
    fn loader_takes_one_action_and_its_own_timeout() {
        assert!(let Ok(cli) = Cli::try_parse_from(["ferrite-dev", "loader", "reboot", "--timeout", "20s"]));
        assert!(let Command::Loader(args) = cli.command);
        check!((args.action, args.timeout) == (LoaderAction::Reboot, Some(Duration::from_secs(20))));
        check!(Cli::try_parse_from(["ferrite-dev", "loader", "restart"]).is_err());
    }

    #[test]
    fn vendor_takes_force_and_defaults_to_the_sibling_crate() {
        assert!(let Ok(cli) = Cli::try_parse_from(["ferrite-dev", "vendor", "--force"]));
        assert!(let Command::Vendor(args) = cli.command);
        check!(args.force);
        check!(args.crate_dir.ends_with("ferrite-wifi-sys"));
    }

    #[test]
    fn stubs_defaults_to_the_sibling_crates_and_takes_check() {
        assert!(let Ok(cli) = Cli::try_parse_from(["ferrite-dev", "stubs", "--check"]));
        assert!(let Command::Stubs(args) = cli.command);
        check!(args.check);
        check!(args.wifi_dir.ends_with("ferrite-wifi-sys") && args.lkpi_dir.ends_with("ferrite-lkpi"));
    }

    #[test]
    fn firmware_defaults_to_the_host_directory_and_the_esp() {
        assert!(let Ok(cli) = Cli::try_parse_from(["ferrite-dev", "firmware", "--list"]));
        assert!(let Command::Firmware(args) = cli.command);
        check!(
            (args.source.as_path(), args.dest.as_path())
                == (std::path::Path::new("/lib/firmware"), std::path::Path::new("esp/efi/ferrite/firmware"))
        );
        check!(args.list && !args.all);
        check!(args.chips.ends_with("ferrite-wifi-sys/firmware-chips.txt"));
    }

    #[test]
    fn run_takes_an_app_and_the_documented_flags() {
        assert!(let Ok(cli) = Cli::try_parse_from(["ferrite-dev", "run", "dbcping", "--log-for", "2m", "--no-capture"]));
        assert!(let Command::Run(args) = cli.command);
        check!((args.app.as_str(), args.log_for, args.no_capture) == ("dbcping", Duration::from_secs(120), true));
        check!(args.timeout == Duration::from_secs(300));
    }
}
