//! `run`: build an app, stage it on the target, reboot into it once, capture, and collect reports.
//!
//! Only `clean-reports`, `install`, `run` and `collect` of the deploy helper are ever invoked:
//! nothing here removes the installed apps or touches firmware boot entries. The one other
//! remote write is clearing the user-owned `<stage_dir>/out/`.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use snafu::{OptionExt, ResultExt, Snafu};

use crate::config::Config;
use crate::link::LinkOptions;
use crate::metadata::{Metadata, MetadataError};
use crate::runner::{Cmd, CmdOutput, OutputMode};

pub const UEFI_TARGET: &str = "x86_64-unknown-uefi";
pub const RECOVERY: &str = "If the target is hung: hold the power button until it turns off, then press it once. It boots its default OS by itself.";
pub const CAPTURE_FILE: &str = "capture.log";
pub const BENCH_APP: &str = "dbcbench";
pub const PING_APP: &str = "dbcping";
pub const PING_PROBE: &str = "probe-from-host";
pub const POLL_EVERY: Duration = Duration::from_secs(2);
pub const PROGRESS_EVERY: Duration = Duration::from_secs(10);
const BOOT_ID_PATH: &str = "/proc/sys/kernel/random/boot_id";
const CONNECT_TIMEOUT_SECS: &str = "3";
/// ssh gives up on a silent server after this many 2 s keepalives, which a vanishing network needs
/// because `ConnectTimeout` stops applying once the connection is up.
const KEEPALIVE: [&str; 4] = ["-o", "ServerAliveInterval=2", "-o", "ServerAliveCountMax=2"];
/// No probe or reboot command gets longer than this, whatever the connection does.
const PROBE_LIMIT: Duration = Duration::from_secs(15);
const REBOOT_LIMIT: Duration = Duration::from_secs(60);

#[derive(Debug, Snafu)]
pub enum RunError {
    #[snafu(display("`{app}` is not a valid app name: use lowercase letters, digits, `-` and `_`, at most 32"))]
    BadAppName { app: String },

    #[snafu(display("cannot start `{cmd}`"))]
    Spawn { cmd: Cmd, source: io::Error },

    #[snafu(display("{step} failed ({status}): `{cmd}`{}", detail(stderr)))]
    Failed { step: &'static str, cmd: Box<Cmd>, status: String, stderr: String },

    #[snafu(display("no ssh host answered: {hosts}"))]
    Unreachable { hosts: String },

    #[snafu(display("`{cmd}` printed no boot id"))]
    NoBootId { cmd: Cmd },

    #[snafu(display("cannot find the package of `{app}`"))]
    Package { app: String, source: MetadataError },

    #[snafu(display("timed out after {}s waiting for {waiting_for}", after.as_secs()))]
    Timeout { waiting_for: &'static str, after: Duration },

    #[snafu(display("cannot start the capture"))]
    Capture { source: io::Error },
}

fn detail(stderr: &str) -> String {
    let stderr = stderr.trim();
    if stderr.is_empty() { String::new() } else { format!(": {stderr}") }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureKind {
    /// Plain log capture, with an optional probe line sent after connecting.
    Log {
        probe: Option<String>,
        duration: Duration,
    },
    Bench {
        wait: Duration,
        link: LinkOptions,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturePlan {
    pub kind: CaptureKind,
    pub file: PathBuf,
}

pub trait Capture {
    /// Stop capturing and report how it ended; `Some` carries an error message.
    fn stop(self: Box<Self>) -> Option<String>;
}

/// Everything `execute` needs from the outside world.
pub trait Env {
    fn run(&self, cmd: &Cmd, mode: OutputMode) -> io::Result<CmdOutput>;
    /// Monotonic time since an arbitrary start.
    fn now(&self) -> Duration;
    fn sleep(&self, duration: Duration);
    fn say(&self, message: &str);
    fn start_capture(&self, plan: &CapturePlan) -> io::Result<Box<dyn Capture>>;
}

#[derive(Debug, Clone)]
pub struct RunPlan {
    pub app: String,
    pub run_dir: PathBuf,
    /// How long the target gets to go down and come back.
    pub timeout: Duration,
    /// Longest a plain log capture runs.
    pub log_for: Duration,
    pub capture: bool,
    /// The transport the benchmark peer uses.
    pub link: LinkOptions,
}

#[derive(Debug)]
pub struct RunResult {
    pub run_dir: PathBuf,
    pub capture_file: Option<PathBuf>,
    pub capture_error: Option<String>,
    pub collected: bool,
}

/// The capture that suits `app`: the benchmark peer for `dbcbench`, else a log, with the probe
/// line for `dbcping`.
pub fn capture_kind(app: &str, log_for: Duration, wait: Duration, link: LinkOptions) -> CaptureKind {
    match app {
        BENCH_APP => CaptureKind::Bench { wait, link },
        PING_APP => CaptureKind::Log { probe: Some(PING_PROBE.to_owned()), duration: log_for },
        _ => CaptureKind::Log { probe: None, duration: log_for },
    }
}

/// The same rule the deploy helper applies to names.
pub fn valid_app_name(app: &str) -> bool {
    let mut chars = app.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && app.len() <= 32
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_'))
}

pub fn execute(env: &impl Env, config: &Config, plan: &RunPlan) -> Result<RunResult, RunError> {
    let app = plan.app.as_str();
    let efi = build_app(env, app)?;

    let first = reach(env, config, app)?;
    env.say(&format!("using ssh host {} (boot id {})", first.host, first.boot_id));
    let commands = Commands::new(config, app, &first.host);

    env.say(&format!("staging {} on {}", efi.display(), first.host));
    checked(env, "scp of the app", &commands.upload(&efi), OutputMode::Collect)?;
    checked(env, "clean-reports", &commands.deploy("clean-reports"), OutputMode::Collect)?;
    checked(env, "install", &commands.deploy_app("install"), OutputMode::Collect)?;

    let capture_file = plan.capture.then(|| plan.run_dir.join(CAPTURE_FILE));
    let capture = match &capture_file {
        Some(file) => {
            let kind = capture_kind(app, plan.log_for, plan.timeout, plan.link);
            Some(env.start_capture(&CapturePlan { kind, file: file.clone() }).context(CaptureSnafu)?)
        },
        None => None,
    };

    let cycle = reboot_cycle(env, config, app, &first, plan.timeout);
    let capture_error = capture.and_then(Capture::stop);
    let back_host = cycle?;

    let commands = Commands::new(config, app, &back_host);
    checked(env, "clearing old reports", &commands.clear_reports(), OutputMode::Collect)?;
    checked(env, "collect", &commands.deploy("collect"), OutputMode::Collect)?;
    let collected = match run_cmd(env, &commands.download(&plan.run_dir), OutputMode::Collect)? {
        out if out.success => true,
        out => {
            env.say(&format!("no reports copied from {back_host}: {}", out.stderr.trim()));
            false
        },
    };
    Ok(RunResult { run_dir: plan.run_dir.clone(), capture_file, capture_error, collected })
}

/// Build `app` for the UEFI target and return the path of its `.efi`.
pub fn build_app(env: &impl Env, app: &str) -> Result<PathBuf, RunError> {
    if !valid_app_name(app) {
        return Err(RunError::BadAppName { app: app.to_owned() });
    }
    env.say(&format!("resolving the package of {app}"));
    let out = checked(env, "cargo metadata", &Commands::metadata(), OutputMode::Collect)?;
    let metadata = Metadata::parse(&out.stdout).context(PackageSnafu { app })?;
    let package = metadata.package_of_bin(app).context(PackageSnafu { app })?.to_owned();

    env.say(&format!("building {app} ({package})"));
    checked(env, "build", &Commands::build(app, &package), OutputMode::Passthrough)?;
    Ok(metadata.target_directory.join(UEFI_TARGET).join("debug").join(format!("{app}.efi")))
}

/// Build `app`, install it on the target and trigger the one-shot reboot into it.
///
/// Returns without waiting for the target: the caller finds out it is up some other way, such as
/// the DbC device appearing. The result is the host that took the commands.
pub fn stage_and_reboot(env: &impl Env, config: &Config, app: &str) -> Result<String, RunError> {
    let efi = build_app(env, app)?;
    let first = reach(env, config, app)?;
    if first.boot_id.is_empty() {
        return Err(RunError::NoBootId { cmd: first.cmd });
    }
    env.say(&format!("using ssh host {} (boot id {})", first.host, first.boot_id));
    let commands = Commands::new(config, app, &first.host);

    env.say(&format!("staging {} on {}", efi.display(), first.host));
    checked(env, "scp of the app", &commands.upload(&efi), OutputMode::Collect)?;
    checked(env, "clean-reports", &commands.deploy("clean-reports"), OutputMode::Collect)?;
    checked(env, "install", &commands.deploy_app("install"), OutputMode::Collect)?;

    env.say(&format!("rebooting {} into {app}", first.host));
    let cmd = commands.reboot();
    let out = run_cmd(env, &cmd, OutputMode::Collect)?;
    if !out.success && out.code != Some(255) && !out.timed_out {
        return Err(failure("run", cmd, &out));
    }
    Ok(first.host)
}

/// A host that answered, with the boot id it reported and the command that read it.
struct Answer {
    host: String,
    boot_id: String,
    cmd: Cmd,
}

/// Read the boot id from the first configured host that answers, trying them in order.
fn probe_hosts(env: &impl Env, config: &Config, app: &str) -> Result<Option<Answer>, RunError> {
    for host in &config.ssh_hosts {
        let cmd = Commands::new(config, app, host).boot_id();
        let out = run_cmd(env, &cmd, OutputMode::Collect)?;
        if out.success {
            return Ok(Some(Answer { host: host.clone(), boot_id: out.stdout.trim().to_owned(), cmd }));
        }
    }
    Ok(None)
}

fn reach(env: &impl Env, config: &Config, app: &str) -> Result<Answer, RunError> {
    probe_hosts(env, config, app)?.context(UnreachableSnafu { hosts: config.ssh_hosts.join(", ") })
}

/// The name progress messages use for the target, whichever address answers.
fn label(config: &Config) -> &str {
    config.host_key_alias.as_deref().or_else(|| config.ssh_hosts.first().map(String::as_str)).unwrap_or("the target")
}

/// Trigger the one-shot reboot on the host that answered, then wait for any host to answer with
/// another boot id. Returns the host that did.
fn reboot_cycle(
    env: &impl Env,
    config: &Config,
    app: &str,
    first: &Answer,
    timeout: Duration,
) -> Result<String, RunError> {
    if first.boot_id.is_empty() {
        return Err(RunError::NoBootId { cmd: first.cmd.clone() });
    }
    env.say(&format!("rebooting {} into {app} (boot id {})", first.host, first.boot_id));
    let cmd = Commands::new(config, app, &first.host).reboot();
    let out = run_cmd(env, &cmd, OutputMode::Collect)?;
    if !out.success && out.code != Some(255) && !out.timed_out {
        return Err(failure("run", cmd, &out));
    }
    wait_for_reboot(env, config, app, &first.boot_id, timeout)
}

/// Poll until some host answers with a boot id other than `before`, and return that host. Seeing
/// the target go unreachable only shapes the progress output: a reboot faster than the poll interval is
/// still detected.
fn wait_for_reboot(
    env: &impl Env,
    config: &Config,
    app: &str,
    before: &str,
    timeout: Duration,
) -> Result<String, RunError> {
    let start = env.now();
    let name = label(config);
    let (mut went_down, mut last_progress) = (false, None::<Duration>);
    loop {
        let answer = probe_hosts(env, config, app)?;
        if let Some(Answer { host, boot_id, .. }) = &answer
            && !boot_id.is_empty()
            && boot_id != before
        {
            env.say(&format!(
                "{name} is back after {}s, answering on {host}",
                env.now().saturating_sub(start).as_secs()
            ));
            return Ok(host.clone());
        }
        if !went_down && answer.is_none() {
            went_down = true;
            env.say(&format!("{name} went down"));
        }
        let elapsed = env.now().saturating_sub(start);
        if elapsed >= timeout {
            env.say(RECOVERY);
            let waiting_for = if went_down { "the target to come back" } else { "the target to reboot" };
            return Err(RunError::Timeout { waiting_for, after: elapsed });
        }
        if last_progress.is_none_or(|at| elapsed.saturating_sub(at) >= PROGRESS_EVERY) {
            last_progress = Some(elapsed);
            let waiting = if went_down { "to come back" } else { "to reboot" };
            env.say(&format!("waiting for {name} {waiting} ({}s)", elapsed.as_secs()));
        }
        env.sleep(POLL_EVERY);
    }
}

fn run_cmd(env: &impl Env, cmd: &Cmd, mode: OutputMode) -> Result<CmdOutput, RunError> {
    env.run(cmd, mode).context(SpawnSnafu { cmd: cmd.clone() })
}

fn checked(env: &impl Env, step: &'static str, cmd: &Cmd, mode: OutputMode) -> Result<CmdOutput, RunError> {
    let out = run_cmd(env, cmd, mode)?;
    if out.success { Ok(out) } else { Err(failure(step, cmd.clone(), &out)) }
}

fn failure(step: &'static str, cmd: Cmd, out: &CmdOutput) -> RunError {
    let status = out.code.map_or_else(|| "killed by a signal".to_owned(), |code| format!("exit {code}"));
    RunError::Failed { step, cmd: Box::new(cmd), status, stderr: out.stderr.clone() }
}

/// Every command line `execute` can issue, for one host.
struct Commands<'a> {
    config: &'a Config,
    app: &'a str,
    host: &'a str,
}

impl<'a> Commands<'a> {
    const fn new(config: &'a Config, app: &'a str, host: &'a str) -> Self {
        Self { config, app, host }
    }

    fn options(&self) -> Vec<String> {
        let mut options: Vec<String> = ["-o", "BatchMode=yes"].map(String::from).to_vec();
        if let Some(alias) = &self.config.host_key_alias {
            options.extend(["-o".to_owned(), format!("HostKeyAlias={alias}")]);
        }
        options
    }

    fn remote(&self, path: &str) -> String {
        format!("{}:{}/{path}", self.host, self.config.stage_dir)
    }

    fn metadata() -> Cmd {
        Cmd::new("cargo", ["metadata", "--format-version", "1", "--no-deps"])
    }

    fn build(app: &str, package: &str) -> Cmd {
        Cmd::new("cargo", ["build", "--target", UEFI_TARGET, "-p", package, "--bin", app])
    }

    fn upload(&self, efi: &Path) -> Cmd {
        let mut args = vec!["-q".to_owned()];
        args.extend(self.options());
        args.extend([efi.to_string_lossy().into_owned(), self.remote(&format!("{}.efi", self.app))]);
        Cmd::new("scp", args)
    }

    fn download(&self, run_dir: &Path) -> Cmd {
        let mut args = vec!["-q".to_owned()];
        args.extend(self.options());
        args.extend([self.remote("out/*"), run_dir.to_string_lossy().into_owned()]);
        Cmd::new("scp", args)
    }

    fn ssh(&self, extra: &[&str], remote: &[&str]) -> Cmd {
        let mut args = self.options();
        args.extend(extra.iter().map(|arg| (*arg).to_owned()));
        args.push(self.host.to_owned());
        args.extend(remote.iter().map(|arg| (*arg).to_owned()));
        Cmd::new("ssh", args)
    }

    fn deploy(&self, subcommand: &str) -> Cmd {
        self.ssh(&[], &["sudo", "-n", self.config.helper.as_str(), subcommand])
    }

    fn deploy_app(&self, subcommand: &str) -> Cmd {
        let mut cmd = self.deploy(subcommand);
        cmd.args.push(self.app.to_owned());
        cmd
    }

    /// The one-shot reboot. Its connection dies with the machine, so it must not wait on it.
    fn reboot(&self) -> Cmd {
        self.ssh(&KEEPALIVE, &["sudo", "-n", self.config.helper.as_str(), "run", self.app]).with_limit(REBOOT_LIMIT)
    }

    fn boot_id(&self) -> Cmd {
        let timeout = format!("ConnectTimeout={CONNECT_TIMEOUT_SECS}");
        let mut extra = vec!["-o", &timeout];
        extra.extend(KEEPALIVE);
        self.ssh(&extra, &["cat", BOOT_ID_PATH]).with_limit(PROBE_LIMIT)
    }

    /// Remove the previous runs' files from the user-owned staging output. The stage directory
    /// is validated to be a plain relative-or-absolute path without `..`, so the glob stays
    /// inside `<stage_dir>/out/`.
    fn clear_reports(&self) -> Cmd {
        let out = format!("{}/out/*", self.config.stage_dir);
        self.ssh(&[], &["rm", "-f", "--", &out])
    }
}

/// The last `n` lines of a report.
pub fn tail_lines(text: &str, n: usize) -> Vec<&str> {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].to_vec()
}

/// The capture's link up and down lines.
pub fn link_lines(text: &str) -> Vec<&str> {
    text.lines().filter(|line| line.ends_with("--- link up ---") || line.ends_with("--- link down ---")).collect()
}

fn has_extension(name: &str, extension: &str) -> bool {
    Path::new(name).extension().is_some_and(|ext| ext.eq_ignore_ascii_case(extension))
}

/// Which collected file is the app's report: `<app>.txt`, else the first `.txt` or `.log`
/// that is not the capture.
pub fn pick_report<'a>(names: &'a [String], app: &str) -> Option<&'a str> {
    let own = format!("{app}.txt");
    names
        .iter()
        .find(|name| **name == own)
        .or_else(|| {
            names
                .iter()
                .find(|name| *name != CAPTURE_FILE && (has_extension(name, "txt") || has_extension(name, "log")))
        })
        .map(String::as_str)
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    use assert2::{assert, check};
    use rstest::rstest;

    use super::*;

    const METADATA: &str = r#"{"target_directory": "/w/target", "packages": [
        {"name": "ferrite-recon", "targets": [{"name": "dbcping", "kind": ["bin"]}, {"name": "dbcbench", "kind": ["bin"]}]}]}"#;

    const DEPLOY: &str = "ssh -o BatchMode=yes target sudo -n /usr/local/sbin/ferrite-esp";
    const REBOOT: &str = "ssh -o BatchMode=yes -o ServerAliveInterval=2 -o ServerAliveCountMax=2 target sudo -n /usr/local/sbin/ferrite-esp run dbcping";

    #[derive(Debug, PartialEq, Eq)]
    enum Event {
        Run(String),
        Say(String),
        CaptureStart(CapturePlan),
        CaptureStop,
    }

    type Events = Rc<RefCell<Vec<Event>>>;

    const BEFORE: Option<&str> = Some("aaaa-1111");
    const AFTER: Option<&str> = Some("bbbb-2222");

    /// Records every call. Each boot id read answers from `answers` in order and then repeats the
    /// last one, with `None` meaning ssh cannot connect; time moves only when `sleep` is called.
    struct Fake {
        events: Events,
        answers: RefCell<Vec<Option<&'static str>>>,
        clock: Cell<Duration>,
        /// Fail any command whose line contains this text.
        fail: Option<&'static str>,
        run_exit: i32,
        /// Hosts whose boot id reads fail with ssh's connection error, until the run command.
        down: RefCell<Vec<&'static str>>,
        /// Whether the run command brings the down hosts back, as a reboot does.
        heal_on_run: bool,
        /// A capture that never finishes by itself, like a benchmark that waits for a device.
        hanging_capture: bool,
    }

    impl Fake {
        fn new(answers: &[Option<&'static str>]) -> Self {
            Self {
                events: Rc::default(),
                answers: RefCell::new(answers.to_vec()),
                clock: Cell::new(Duration::ZERO),
                fail: None,
                run_exit: 0,
                down: RefCell::default(),
                heal_on_run: true,
                hanging_capture: false,
            }
        }

        fn boot_id_reads(&self) -> usize {
            self.commands().iter().filter(|c| c.ends_with(BOOT_ID_PATH)).count()
        }

        fn commands(&self) -> Vec<String> {
            self.events
                .borrow()
                .iter()
                .filter_map(|e| if let Event::Run(c) = e { Some(c.clone()) } else { None })
                .collect()
        }

        fn said(&self) -> Vec<String> {
            self.events
                .borrow()
                .iter()
                .filter_map(|e| if let Event::Say(m) = e { Some(m.clone()) } else { None })
                .collect()
        }
    }

    /// A capture; a hanging one keeps running on its own thread until it is stopped.
    struct FakeCapture {
        events: Events,
        hang: Option<(Arc<AtomicBool>, thread::JoinHandle<()>)>,
    }

    impl Capture for FakeCapture {
        fn stop(self: Box<Self>) -> Option<String> {
            if let Some((release, thread)) = self.hang {
                release.store(true, Ordering::Relaxed);
                thread.join().expect("the hanging capture thread");
            }
            self.events.borrow_mut().push(Event::CaptureStop);
            None
        }
    }

    impl Env for Fake {
        fn run(&self, cmd: &Cmd, _: OutputMode) -> io::Result<CmdOutput> {
            let text = cmd.to_string();
            self.events.borrow_mut().push(Event::Run(text.clone()));
            let reply = |success: bool, code: i32, stdout: &str, stderr: &str| {
                Ok(CmdOutput {
                    success,
                    code: Some(code),
                    stdout: stdout.into(),
                    stderr: stderr.into(),
                    ..CmdOutput::default()
                })
            };
            if self.fail.is_some_and(|needle| text.contains(needle)) {
                return reply(false, 1, "", "boom\n");
            }
            if text.starts_with("cargo metadata") {
                return reply(true, 0, METADATA, "");
            }
            if text.ends_with("ferrite-esp run dbcping") {
                if self.heal_on_run {
                    self.down.borrow_mut().clear();
                }
                return reply(self.run_exit == 0, self.run_exit, "", "");
            }
            if text.ends_with(BOOT_ID_PATH) {
                if self.down.borrow().iter().any(|host| text.contains(&format!(" {host} cat "))) {
                    return reply(false, 255, "", "");
                }
                let mut answers = self.answers.borrow_mut();
                let answer = if answers.len() > 1 { answers.remove(0) } else { answers.first().copied().flatten() };
                return answer.map_or_else(|| reply(false, 255, "", ""), |id| reply(true, 0, &format!("{id}\n"), ""));
            }
            reply(true, 0, "", "")
        }

        fn now(&self) -> Duration {
            self.clock.get()
        }

        fn sleep(&self, duration: Duration) {
            self.clock.set(self.clock.get() + duration);
        }

        fn say(&self, message: &str) {
            self.events.borrow_mut().push(Event::Say(message.to_owned()));
        }

        fn start_capture(&self, plan: &CapturePlan) -> io::Result<Box<dyn Capture>> {
            self.events.borrow_mut().push(Event::CaptureStart(plan.clone()));
            let hang = self.hanging_capture.then(|| {
                let release = Arc::new(AtomicBool::new(false));
                let flag = Arc::clone(&release);
                let thread = thread::spawn(move || {
                    while !flag.load(Ordering::Relaxed) {
                        thread::sleep(Duration::from_millis(1));
                    }
                });
                (release, thread)
            });
            Ok(Box::new(FakeCapture { events: Rc::clone(&self.events), hang }))
        }
    }

    fn plan(app: &str) -> RunPlan {
        RunPlan {
            app: app.into(),
            run_dir: "/runs/x".into(),
            timeout: Duration::from_secs(300),
            log_for: Duration::from_secs(120),
            capture: true,
            link: LinkOptions::default(),
        }
    }

    #[test]
    fn the_cycle_issues_exactly_these_commands_in_this_order() {
        let fake = Fake::new(&[BEFORE, BEFORE, None, None, AFTER]);
        assert!(let Ok(result) = execute(&fake, &Config::default(), &plan("dbcping")));

        let probe = "ssh -o BatchMode=yes -o ConnectTimeout=3 -o ServerAliveInterval=2 -o ServerAliveCountMax=2 target cat /proc/sys/kernel/random/boot_id".to_owned();
        check!(
            fake.commands()
                == [
                    "cargo metadata --format-version 1 --no-deps".to_owned(),
                    "cargo build --target x86_64-unknown-uefi -p ferrite-recon --bin dbcping".into(),
                    probe.clone(),
                    "scp -q -o BatchMode=yes /w/target/x86_64-unknown-uefi/debug/dbcping.efi target:ferrite-stage/dbcping.efi"
                        .into(),
                    format!("{DEPLOY} clean-reports"),
                    format!("{DEPLOY} install dbcping"),
                    REBOOT.to_owned(),
                    probe.clone(),
                    probe.clone(),
                    probe.clone(),
                    probe,
                    "ssh -o BatchMode=yes target rm -f -- ferrite-stage/out/*".into(),
                    format!("{DEPLOY} collect"),
                    "scp -q -o BatchMode=yes target:ferrite-stage/out/* /runs/x".into(),
                ]
        );
        check!(fake.commands().iter().all(|c| !c.contains("uninstall")));
        check!(fake.said().iter().any(|m| m.contains("aaaa-1111")), "the boot id before the reboot is reported");
        check!(result.collected);
        check!(result.capture_file == Some(PathBuf::from("/runs/x/capture.log")));
    }

    #[test]
    fn the_capture_brackets_the_reboot_and_stops_before_collecting() {
        let fake = Fake::new(&[BEFORE, None, AFTER]);
        assert!(let Ok(_) = execute(&fake, &Config::default(), &plan("dbcping")));
        let events = fake.events.borrow();
        let at = |pred: &dyn Fn(&Event) -> bool| events.iter().position(pred).expect("event present");
        let install = at(&|e| matches!(e, Event::Run(c) if c.ends_with("install dbcping")));
        let start = at(&|e| matches!(e, Event::CaptureStart(_)));
        let run = at(&|e| matches!(e, Event::Run(c) if c.ends_with("ferrite-esp run dbcping")));
        let stop = at(&|e| matches!(e, Event::CaptureStop));
        let collect = at(&|e| matches!(e, Event::Run(c) if c.ends_with("ferrite-esp collect")));
        check!(install < start && start < run && run < stop && stop < collect);
        assert!(let Event::CaptureStart(started) = &events[start]);
        check!(
            started.kind
                == CaptureKind::Log { probe: Some("probe-from-host".into()), duration: Duration::from_secs(120) }
        );
    }

    #[test]
    fn a_target_that_never_comes_back_times_out_with_the_recovery_step_and_no_collect() {
        let fake = Fake::new(&[BEFORE, None]);
        assert!(let Err(RunError::Timeout { waiting_for, after }) = execute(&fake, &Config::default(), &plan("dbcping")));
        check!(waiting_for == "the target to come back");
        check!(after >= Duration::from_secs(300));
        check!(fake.said().last().map(String::as_str) == Some(RECOVERY));
        check!(
            RECOVERY
                == "If the target is hung: hold the power button until it turns off, then press it once. It boots its default OS by itself."
        );
        check!(fake.commands().iter().all(|c| !c.ends_with("ferrite-esp collect")));
        check!(fake.events.borrow().contains(&Event::CaptureStop), "the capture is stopped on the way out");
    }

    #[test]
    fn a_target_whose_boot_id_never_changes_times_out_too() {
        let fake = Fake::new(&[BEFORE]);
        assert!(let Err(RunError::Timeout { waiting_for, .. }) = execute(&fake, &Config::default(), &plan("dbcping")));
        check!(waiting_for == "the target to reboot");
        check!(fake.said().iter().any(|m| m == RECOVERY));
    }

    #[test]
    fn a_reboot_faster_than_the_poll_is_still_detected() {
        let fake = Fake::new(&[BEFORE, BEFORE, AFTER]);
        assert!(let Ok(_) = execute(&fake, &Config::default(), &plan("dbcping")));
        check!(fake.said().iter().all(|m| !m.contains("went down")), "ssh was never seen unreachable");
        check!(fake.said().iter().any(|m| m.contains("is back")));
        check!(fake.boot_id_reads() == 3);
        check!(fake.commands().iter().any(|c| c.ends_with("ferrite-esp collect")));
    }

    #[test]
    fn answering_with_the_old_boot_id_after_an_outage_is_not_a_reboot() {
        let fake = Fake::new(&[BEFORE, None, BEFORE, BEFORE, AFTER]);
        assert!(let Ok(_) = execute(&fake, &Config::default(), &plan("dbcping")));
        check!(fake.boot_id_reads() == 5, "a network blip does not end the wait");
    }

    #[test]
    fn an_empty_boot_id_stops_before_the_reboot() {
        let fake = Fake::new(&[Some("")]);
        assert!(let Err(RunError::NoBootId { .. }) = execute(&fake, &Config::default(), &plan("dbcping")));
        check!(fake.commands().iter().all(|c| !c.ends_with("ferrite-esp run dbcping")));
        check!(fake.events.borrow().contains(&Event::CaptureStop));
    }

    #[test]
    fn progress_is_printed_at_most_every_ten_seconds() {
        let fake = Fake::new(&[BEFORE, None]);
        let _ = execute(&fake, &Config::default(), &RunPlan { timeout: Duration::from_secs(100), ..plan("dbcping") });
        let progress = fake.said().iter().filter(|m| m.starts_with("waiting for")).count();
        check!((9..=11).contains(&progress), "{progress} progress lines over 100 s");
    }

    #[rstest]
    #[case::ssh_dropped_by_the_reboot(255, true)]
    #[case::clean_exit(0, true)]
    #[case::helper_refused(1, false)]
    fn the_run_command_may_lose_its_connection_but_not_fail(#[case] exit: i32, #[case] proceeds: bool) {
        let mut fake = Fake::new(&[BEFORE, None, AFTER]);
        fake.run_exit = exit;
        let result = execute(&fake, &Config::default(), &plan("dbcping"));
        check!(result.is_ok() == proceeds);
        check!((fake.boot_id_reads() > 1) == proceeds, "polling only after a started reboot");
        check!(fake.events.borrow().contains(&Event::CaptureStop));
    }

    #[test]
    fn a_failed_build_stops_before_anything_reaches_the_target() {
        let mut fake = Fake::new(&[BEFORE]);
        fake.fail = Some("cargo build");
        assert!(let Err(RunError::Failed { step, .. }) = execute(&fake, &Config::default(), &plan("dbcping")));
        check!(step == "build");
        check!(fake.commands().len() == 2);
        check!(!fake.events.borrow().iter().any(|e| matches!(e, Event::CaptureStart(_))));
    }

    #[test]
    fn an_unknown_binary_fails_at_package_lookup() {
        let fake = Fake::new(&[BEFORE]);
        assert!(let Err(RunError::Package { .. }) = execute(&fake, &Config::default(), &plan("nosuchapp")));
        check!(fake.commands() == ["cargo metadata --format-version 1 --no-deps"]);
    }

    #[test]
    fn no_capture_means_no_capture() {
        let fake = Fake::new(&[BEFORE, None, AFTER]);
        assert!(let Ok(result) = execute(&fake, &Config::default(), &RunPlan { capture: false, ..plan("dbcping") }));
        check!(result.capture_file.is_none());
        check!(!fake.events.borrow().iter().any(|e| matches!(e, Event::CaptureStart(_) | Event::CaptureStop)));
    }

    #[test]
    fn a_failed_report_copy_is_not_fatal() {
        let mut fake = Fake::new(&[BEFORE, None, AFTER]);
        fake.fail = Some("target:ferrite-stage/out/*");
        assert!(let Ok(result) = execute(&fake, &Config::default(), &plan("dbcping")));
        check!(!result.collected);
    }

    #[test]
    fn a_failed_clear_of_old_reports_stops_before_collecting() {
        let mut fake = Fake::new(&[BEFORE, None, AFTER]);
        fake.fail = Some("rm -f --");
        assert!(let Err(RunError::Failed { step, .. }) = execute(&fake, &Config::default(), &plan("dbcping")));
        check!(step == "clearing old reports");
        check!(fake.commands().iter().all(|c| !c.ends_with("ferrite-esp collect")));
    }

    #[test]
    fn config_values_reach_the_command_lines() {
        let config = Config {
            ssh_hosts: vec!["laptop".into()],
            helper: "/opt/deploy".into(),
            stage_dir: "stage".into(),
            ..Config::default()
        };
        let fake = Fake::new(&[BEFORE, None, AFTER]);
        let _ = execute(&fake, &config, &plan("dbcping"));
        let commands = fake.commands();
        check!(commands.contains(&"ssh -o BatchMode=yes laptop sudo -n /opt/deploy install dbcping".to_owned()));
        check!(commands.iter().any(|c| c.ends_with("laptop:stage/dbcping.efi")));
        check!(commands.contains(&"ssh -o BatchMode=yes laptop rm -f -- stage/out/*".to_owned()));
    }

    #[rstest]
    #[case::bench("dbcbench", CaptureKind::Bench { wait: Duration::from_secs(300), link: LinkOptions::default() })]
    #[case::ping("dbcping", CaptureKind::Log { probe: Some("probe-from-host".into()), duration: Duration::from_secs(120) })]
    #[case::other("xhciscan", CaptureKind::Log { probe: None, duration: Duration::from_secs(120) })]
    fn the_capture_follows_the_app(#[case] app: &str, #[case] want: CaptureKind) {
        let link = LinkOptions::default();
        check!(capture_kind(app, Duration::from_secs(120), Duration::from_secs(300), link) == want);
    }

    const LAN: &str = "192.0.2.10";

    fn two_hosts() -> Config {
        Config {
            ssh_hosts: vec![LAN.into(), "target".into()],
            host_key_alias: Some("target-key".into()),
            ..Config::default()
        }
    }

    fn via(host: &str, tail: &str) -> String {
        format!("ssh -o BatchMode=yes -o HostKeyAlias=target-key {host} {tail}")
    }

    fn reboot_via(host: &str) -> String {
        format!(
            "ssh -o BatchMode=yes -o HostKeyAlias=target-key -o ServerAliveInterval=2 -o ServerAliveCountMax=2 {host} sudo -n /usr/local/sbin/ferrite-esp run dbcping"
        )
    }

    fn probe_of(host: &str) -> String {
        format!(
            "ssh -o BatchMode=yes -o HostKeyAlias=target-key -o ConnectTimeout=3 -o ServerAliveInterval=2 -o ServerAliveCountMax=2 {host} cat {BOOT_ID_PATH}"
        )
    }

    const SETUP: [&str; 2] = [
        "cargo metadata --format-version 1 --no-deps",
        "cargo build --target x86_64-unknown-uefi -p ferrite-recon --bin dbcping",
    ];

    #[test]
    fn the_first_host_that_answers_serves_every_step_and_the_alias_reaches_every_ssh_and_scp() {
        let fake = Fake::new(&[BEFORE, AFTER]);
        assert!(let Ok(_) = execute(&fake, &two_hosts(), &plan("dbcping")));
        let helper = "sudo -n /usr/local/sbin/ferrite-esp";
        let mut want: Vec<String> = SETUP.map(String::from).to_vec();
        want.extend([
            probe_of(LAN),
            format!("scp -q -o BatchMode=yes -o HostKeyAlias=target-key /w/target/x86_64-unknown-uefi/debug/dbcping.efi {LAN}:ferrite-stage/dbcping.efi"),
            via(LAN, &format!("{helper} clean-reports")),
            via(LAN, &format!("{helper} install dbcping")),
            reboot_via(LAN),
            probe_of(LAN),
            via(LAN, "rm -f -- ferrite-stage/out/*"),
            via(LAN, &format!("{helper} collect")),
            format!("scp -q -o BatchMode=yes -o HostKeyAlias=target-key {LAN}:ferrite-stage/out/* /runs/x"),
        ]);
        check!(fake.commands() == want);
        check!(fake.said().iter().any(|m| m == &format!("using ssh host {LAN} (boot id aaaa-1111)")));
        check!(
            fake.said()
                .iter()
                .any(|m| m.starts_with("target-key is back after") && m.ends_with(&format!("answering on {LAN}")))
        );
    }

    #[test]
    fn an_unreachable_first_host_falls_back_and_the_first_host_is_preferred_again_after_the_reboot() {
        let fake = Fake::new(&[BEFORE, AFTER]);
        fake.down.borrow_mut().push(LAN);
        assert!(let Ok(_) = execute(&fake, &two_hosts(), &plan("dbcping")));
        let helper = "sudo -n /usr/local/sbin/ferrite-esp";
        let mut want: Vec<String> = SETUP.map(String::from).to_vec();
        want.extend([
            probe_of(LAN),
            probe_of("target"),
            "scp -q -o BatchMode=yes -o HostKeyAlias=target-key /w/target/x86_64-unknown-uefi/debug/dbcping.efi target:ferrite-stage/dbcping.efi".to_owned(),
            via("target", &format!("{helper} clean-reports")),
            via("target", &format!("{helper} install dbcping")),
            reboot_via("target"),
            probe_of(LAN),
            via(LAN, "rm -f -- ferrite-stage/out/*"),
            via(LAN, &format!("{helper} collect")),
            format!("scp -q -o BatchMode=yes -o HostKeyAlias=target-key {LAN}:ferrite-stage/out/* /runs/x"),
        ]);
        check!(fake.commands() == want);
        check!(fake.said().iter().any(|m| m == "using ssh host target (boot id aaaa-1111)"));
    }

    #[test]
    fn every_poll_tries_the_next_host_after_a_failed_one() {
        let mut fake = Fake::new(&[BEFORE, BEFORE, AFTER]);
        fake.heal_on_run = false;
        fake.down.borrow_mut().push(LAN);
        assert!(let Ok(_) = execute(&fake, &two_hosts(), &plan("dbcping")));
        let probes: Vec<String> = fake.commands().into_iter().filter(|c| c.ends_with(BOOT_ID_PATH)).collect();
        check!(probes == [probe_of(LAN), probe_of("target")].into_iter().cycle().take(6).collect::<Vec<_>>());
        check!(fake.said().iter().any(|m| m.ends_with("answering on target")));
    }

    #[test]
    fn a_capture_that_never_ends_does_not_hold_up_the_boot_id_wait() {
        let mut fake = Fake::new(&[BEFORE, None, None, AFTER]);
        fake.hanging_capture = true;
        assert!(let Ok(result) = execute(&fake, &Config::default(), &plan("dbcping")));
        check!(result.collected);
        let events = fake.events.borrow();
        let said = |text: &str| events.iter().position(|e| matches!(e, Event::Say(m) if m.contains(text)));
        let stopped = events.iter().position(|e| *e == Event::CaptureStop);
        check!(said("went down").is_some() && said("waiting for target to come back").is_some());
        check!(said("is back after").is_some() && said("is back after") < stopped);
    }

    #[test]
    fn the_commands_that_outlive_the_network_are_bounded() {
        let config = Config::default();
        let commands = Commands::new(&config, "dbcping", "target");
        let (probe, reboot) = (commands.boot_id(), commands.reboot());
        check!(probe.limit == Some(PROBE_LIMIT) && reboot.limit == Some(REBOOT_LIMIT));
        check!(
            [&probe, &reboot].iter().all(|c| c.to_string().contains("ServerAliveInterval=2 -o ServerAliveCountMax=2"))
        );
        check!(commands.deploy("collect").limit.is_none());
    }

    #[test]
    fn a_target_that_answers_on_no_host_stops_before_staging() {
        let fake = Fake::new(&[BEFORE]);
        fake.down.borrow_mut().extend([LAN, "target"]);
        assert!(let Err(e) = execute(&fake, &two_hosts(), &plan("dbcping")));
        check!(e.to_string() == format!("no ssh host answered: {LAN}, target"));
        check!(fake.commands().len() == 4, "{:?}", fake.commands());
        check!(!fake.events.borrow().iter().any(|e| matches!(e, Event::CaptureStart(_))));
    }

    #[test]
    fn the_transport_choice_reaches_the_benchmark_capture() {
        let fake = Fake::new(&[BEFORE, None, AFTER]);
        let link = LinkOptions { transport: crate::link::Transport::Tty, ..LinkOptions::default() };
        assert!(let Ok(_) = execute(&fake, &Config::default(), &RunPlan { link, ..plan("dbcbench") }));
        let events = fake.events.borrow();
        assert!(let Some(Event::CaptureStart(started)) = events.iter().find(|e| matches!(e, Event::CaptureStart(_))));
        check!(started.kind == CaptureKind::Bench { wait: Duration::from_secs(300), link });
    }

    #[rstest]
    #[case::plain("dbcping", true)]
    #[case::digits_dash_underscore("a1-b_c", true)]
    #[case::empty("", false)]
    #[case::leading_dash("-x", false)]
    #[case::upper("Ping", false)]
    #[case::injection("ping; reboot", false)]
    #[case::too_long("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", false)]
    fn app_names_follow_the_helpers_rule(#[case] app: &str, #[case] ok: bool) {
        check!(valid_app_name(app) == ok);
    }

    #[test]
    fn a_bad_name_is_refused_before_any_command_runs() {
        let fake = Fake::new(&[BEFORE]);
        assert!(let Err(RunError::BadAppName { .. }) = execute(&fake, &Config::default(), &plan("x; rm -rf ~")));
        check!(fake.commands().is_empty());
    }

    #[test]
    fn report_helpers_pick_tail_links_and_file() {
        let text = "a\nb\nc\nd";
        check!(tail_lines(text, 2) == ["c", "d"]);
        check!(tail_lines(text, 10).len() == 4);
        let capture = "12:00:00.001 --- link up ---\n12:00:01.000 hello\n12:00:09.500 --- link down ---\n";
        check!(link_lines(capture) == ["12:00:00.001 --- link up ---", "12:00:09.500 --- link down ---"]);
        let names: Vec<String> = ["capture.log", "other.txt", "dbcping.txt"].map(String::from).to_vec();
        check!(pick_report(&names, "dbcping") == Some("dbcping.txt"));
        check!(pick_report(&names, "xhciscan") == Some("other.txt"));
        check!(pick_report(&names[..1], "xhciscan").is_none());
    }

    #[test]
    fn staging_for_a_boot_stops_at_the_reboot_and_never_polls_for_the_target() {
        let fake = Fake::new(&[BEFORE]);
        assert!(let Ok(host) = stage_and_reboot(&fake, &Config::default(), "dbcping"));
        check!(host == "target");
        let commands = fake.commands();
        check!(commands.last().map(String::as_str) == Some(REBOOT));
        check!(commands[1].starts_with("cargo build --target x86_64-unknown-uefi -p ferrite-recon --bin dbcping"));
        check!(fake.boot_id_reads() == 1, "only the read before the reboot");
        check!(commands.iter().all(|c| !c.contains("collect") && !c.contains("uninstall")));
    }
}
