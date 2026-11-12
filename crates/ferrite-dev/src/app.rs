//! Running the parsed command line.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use ferrite_wire::link::{DEFAULT_WATCHDOG_SECS, PushKind};
use signal_hook::consts::{SIGINT, SIGTERM};

use snafu::ResultExt;

use crate::cli::{BenchArgs, Cli, Command, LoaderArgs, LogArgs, PushArgs, RunArgs, StubsArgs, VendorArgs};
use crate::config::{self, Config, ConfigError};
use crate::emit::Emitter;
use crate::error::{
    BenchSnafu, Error, LoaderSnafu, LogSnafu, OutputSnafu, PushSnafu, RunSnafu, StubsSnafu, VendorSnafu,
};
use crate::live::Live;
use crate::loader::{self, LoaderOptions};
use crate::push::{self, PushOptions};
use crate::run::{self, RunPlan, RunResult};
use crate::stubs::{self, StubsOptions};
use crate::vendor::{self, VendorOptions};
use crate::{bench, log};

const REPORT_TAIL_LINES: usize = 15;

pub fn dispatch(cli: Cli) -> Result<(), Error> {
    let (config, source) = load_config(cli.config.as_deref())?;
    match cli.command {
        Command::Config => {
            let origin = source.map_or_else(|| "built-in defaults".to_owned(), |path| path.display().to_string());
            println!("# effective configuration, from {origin}");
            print!("{}", config.render()?);
            Ok(())
        },
        Command::Log(args) => log_command(&config, &args),
        Command::Bench(args) => bench_command(&config, &args),
        Command::Run(args) => run_command(&config, &args),
        Command::Push(args) => push_command(&config, &args),
        Command::Loader(args) => loader_command(&config, &args),
        Command::Vendor(args) => vendor_command(&args),
        Command::Stubs(args) => stubs_command(&args),
    }
}

/// The config and the file it came from, when there is one.
fn load_config(explicit: Option<&Path>) -> Result<(Config, Option<PathBuf>), ConfigError> {
    let (path, required) = match explicit {
        Some(path) => (path.to_owned(), true),
        None => (config::default_path(|name| std::env::var_os(name))?, false),
    };
    let config = Config::load(&path, required)?;
    Ok((config, path.exists().then_some(path)))
}

fn emitter(out: Option<&Path>) -> Result<Emitter, Error> {
    let emitter = Emitter::new().with_stdout();
    match out {
        Some(path) => emitter.with_file(path).context(OutputSnafu { path }),
        None => Ok(emitter),
    }
}

fn log_command(config: &Config, args: &LogArgs) -> Result<(), Error> {
    let options = log::LogOptions { duration: args.duration, probe: args.probe.clone() };
    log::run(config, &options, &mut emitter(args.out.as_deref())?, &AtomicBool::new(false)).context(LogSnafu)
}

fn bench_command(config: &Config, args: &BenchArgs) -> Result<(), Error> {
    let options = bench::BenchOptions { phases: args.phases.clone(), wait: args.wait, link: args.link.options() };
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [SIGINT, SIGTERM] {
        if let Err(e) = signal_hook::flag::register(signal, Arc::clone(&stop)) {
            eprintln!(
                "ferrite-dev: cannot catch signal {signal}, so an interrupt will not hand the USB device back: {e}"
            );
        }
    }
    bench::run(config, &options, &mut emitter(args.out.as_deref())?, &stop).context(BenchSnafu)
}

/// A flag that an interrupt sets, so the USB device is handed back to the kernel on the way out.
fn stop_on_signals() -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [SIGINT, SIGTERM] {
        if let Err(e) = signal_hook::flag::register(signal, Arc::clone(&stop)) {
            eprintln!(
                "ferrite-dev: cannot catch signal {signal}, so an interrupt will not hand the USB device back: {e}"
            );
        }
    }
    stop
}

fn push_command(config: &Config, args: &PushArgs) -> Result<(), Error> {
    let options = PushOptions {
        app: args.app.clone(),
        kind: if args.loader { PushKind::Loader } else { PushKind::App },
        watchdog: args.watchdog.unwrap_or_else(|| Duration::from_secs(DEFAULT_WATCHDOG_SECS.into())),
        wait: args.wait,
        boot: args.boot.then_some(args.boot_timeout),
        reboot: args.reboot,
        stop_after: args.stop_after,
        out: args.out.clone(),
        link: args.link.options(),
    };
    push::run(config, &options, &stop_on_signals()).context(PushSnafu)
}

fn loader_command(config: &Config, args: &LoaderArgs) -> Result<(), Error> {
    let options = LoaderOptions {
        action: args.action,
        timeout: args.timeout.unwrap_or_else(|| args.action.default_timeout()),
        link: args.link.options(),
    };
    loader::run(config, &options, &stop_on_signals()).context(LoaderSnafu)
}

fn vendor_command(args: &VendorArgs) -> Result<(), Error> {
    let options = VendorOptions::from_env(args.crate_dir.clone(), args.force).context(VendorSnafu)?;
    vendor::run(&options).context(VendorSnafu)
}

fn stubs_command(args: &StubsArgs) -> Result<(), Error> {
    let options =
        StubsOptions::from_env(args.wifi_dir.clone(), args.lkpi_dir.clone(), args.check).context(StubsSnafu)?;
    println!("{}", stubs::run(&options).context(StubsSnafu)?);
    Ok(())
}

fn run_command(config: &Config, args: &RunArgs) -> Result<(), Error> {
    let stamp = jiff::Zoned::now().strftime("%Y%m%d-%H%M%S");
    let base = std::path::absolute(&config.run_dir).context(OutputSnafu { path: config.run_dir.clone() })?;
    let run_dir = base.join(format!("{stamp}-{}", args.app));
    fs::create_dir_all(&run_dir).context(OutputSnafu { path: run_dir.clone() })?;
    let plan = RunPlan {
        app: args.app.clone(),
        run_dir,
        timeout: args.timeout,
        log_for: args.log_for,
        capture: !args.no_capture,
        link: args.link.options(),
    };
    let result = run::execute(&Live::new(config), config, &plan).context(RunSnafu)?;
    print_result(&plan.app, &result);
    Ok(())
}

fn print_result(app: &str, result: &RunResult) {
    println!("--- result ---");
    println!("run directory: {}", result.run_dir.display());
    if let Some(file) = &result.capture_file {
        println!("capture: {}", file.display());
    }
    if let Some(error) = &result.capture_error {
        println!("capture ended with an error: {error}");
    }
    if !result.collected {
        println!("no reports were copied from the target");
    }
    let names: Vec<String> = fs::read_dir(&result.run_dir)
        .map_or_default(|entries| entries.flatten().filter_map(|e| e.file_name().into_string().ok()).collect());
    if let Some(report) = run::pick_report(&names, app) {
        let path = result.run_dir.join(report);
        println!("report: {}", path.display());
        match fs::read_to_string(&path) {
            Ok(text) => run::tail_lines(&text, REPORT_TAIL_LINES).iter().for_each(|line| println!("  {line}")),
            Err(e) => println!("  cannot read it: {e}"),
        }
    }
    if let Some(Ok(text)) = result.capture_file.as_deref().map(fs::read_to_string) {
        let links = run::link_lines(&text);
        println!("link events: {}", if links.is_empty() { "none" } else { "" });
        for line in links {
            println!("  {line}");
        }
    }
}
