//! `run`: build an app, stage it on the target, reboot into it once, capture, and collect reports.
//!
//! Only `clean-reports`, `install`, `run` and `collect` of the deploy helper are ever invoked:
//! nothing here removes the installed apps or touches firmware boot entries. The one other
//! remote write is clearing the user-owned `<stage_dir>/out/`.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use snafu::{ResultExt, Snafu};

use crate::config::Config;
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

#[derive(Debug, Snafu)]
pub enum RunError {
    #[snafu(display("`{app}` is not a valid app name: use lowercase letters, digits, `-` and `_`, at most 32"))]
    BadAppName { app: String },

    #[snafu(display("cannot start `{cmd}`"))]
    Spawn { cmd: Cmd, source: io::Error },

    #[snafu(display("{step} failed ({status}): `{cmd}`{}", detail(stderr)))]
    Failed { step: &'static str, cmd: Cmd, status: String, stderr: String },

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
pub fn capture_kind(app: &str, log_for: Duration, wait: Duration) -> CaptureKind {
    match app {
        BENCH_APP => CaptureKind::Bench { wait },
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
    if !valid_app_name(app) {
        return Err(RunError::BadAppName { app: app.to_owned() });
    }
    let commands = Commands { config, app };

    env.say(&format!("resolving the package of {app}"));
    let cmd = Commands::metadata();
    let out = checked(env, "cargo metadata", &cmd, OutputMode::Collect)?;
    let metadata = Metadata::parse(&out.stdout).context(PackageSnafu { app })?;
    let package = metadata.package_of_bin(app).context(PackageSnafu { app })?.to_owned();

    env.say(&format!("building {app} ({package})"));
    checked(env, "build", &commands.build(&package), OutputMode::Passthrough)?;

    let efi = metadata.target_directory.join(UEFI_TARGET).join("debug").join(format!("{app}.efi"));
    env.say(&format!("staging {} on {}", efi.display(), config.ssh_host));
    checked(env, "scp of the app", &commands.upload(&efi), OutputMode::Collect)?;
    checked(env, "clean-reports", &commands.deploy("clean-reports"), OutputMode::Collect)?;
    checked(env, "install", &commands.deploy_app("install"), OutputMode::Collect)?;

    let capture_file = plan.capture.then(|| plan.run_dir.join(CAPTURE_FILE));
    let capture = match &capture_file {
        Some(file) => {
            let kind = capture_kind(app, plan.log_for, plan.timeout);
            Some(env.start_capture(&CapturePlan { kind, file: file.clone() }).context(CaptureSnafu)?)
        },
        None => None,
    };

    let cycle = reboot_cycle(env, &commands, plan.timeout);
    let capture_error = capture.and_then(Capture::stop);
    cycle?;

    checked(env, "clearing old reports", &commands.clear_reports(), OutputMode::Collect)?;
    checked(env, "collect", &commands.deploy("collect"), OutputMode::Collect)?;
    let collected = match run_cmd(env, &commands.download(&plan.run_dir), OutputMode::Collect)? {
        out if out.success => true,
        out => {
            env.say(&format!("no reports copied from {}: {}", config.ssh_host, out.stderr.trim()));
            false
        },
    };
    Ok(RunResult { run_dir: plan.run_dir.clone(), capture_file, capture_error, collected })
}

/// Note the target's boot id, trigger the one-shot reboot, then wait for ssh to answer with another one.
fn reboot_cycle(env: &impl Env, commands: &Commands<'_>, timeout: Duration) -> Result<(), RunError> {
    let probe = commands.boot_id();
    let before = checked(env, "reading the boot id", &probe, OutputMode::Collect)?.stdout.trim().to_owned();
    if before.is_empty() {
        return Err(RunError::NoBootId { cmd: probe });
    }
    env.say(&format!("rebooting {} into {} (boot id {before})", commands.config.ssh_host, commands.app));
    let cmd = commands.deploy_app("run");
    let out = run_cmd(env, &cmd, OutputMode::Collect)?;
    if !out.success && out.code != Some(255) {
        return Err(failure("run", cmd, &out));
    }
    wait_for_reboot(env, commands, &before, timeout)
}

/// Poll until ssh answers with a boot id other than `before`. Seeing the target go unreachable only
/// shapes the progress output: a reboot faster than the poll interval is still detected.
fn wait_for_reboot(env: &impl Env, commands: &Commands<'_>, before: &str, timeout: Duration) -> Result<(), RunError> {
    let start = env.now();
    let probe = commands.boot_id();
    let (mut went_down, mut last_progress) = (false, None::<Duration>);
    loop {
        let out = run_cmd(env, &probe, OutputMode::Collect)?;
        let reachable = out.success;
        let now_id = out.stdout.trim();
        if reachable && !now_id.is_empty() && now_id != before {
            env.say(&format!(
                "{} is back after {}s",
                commands.config.ssh_host,
                env.now().saturating_sub(start).as_secs()
            ));
            return Ok(());
        }
        if !went_down && !reachable {
            went_down = true;
            env.say(&format!("{} went down", commands.config.ssh_host));
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
            env.say(&format!("waiting for {} {waiting} ({}s)", commands.config.ssh_host, elapsed.as_secs()));
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
    RunError::Failed { step, cmd, status, stderr: out.stderr.clone() }
}

/// Every command line `execute` can issue.
struct Commands<'a> {
    config: &'a Config,
    app: &'a str,
}

impl Commands<'_> {
    fn ssh_args(&self) -> [&str; 3] {
        ["-o", "BatchMode=yes", self.config.ssh_host.as_str()]
    }

    fn remote(&self, path: &str) -> String {
        format!("{}:{}/{path}", self.config.ssh_host, self.config.stage_dir)
    }

    fn metadata() -> Cmd {
        Cmd::new("cargo", ["metadata", "--format-version", "1", "--no-deps"])
    }

    fn build(&self, package: &str) -> Cmd {
        Cmd::new("cargo", ["build", "--target", UEFI_TARGET, "-p", package, "--bin", self.app])
    }

    fn upload(&self, efi: &Path) -> Cmd {
        let [o, opt, _] = self.ssh_args();
        Cmd::new("scp", ["-q", o, opt, &efi.to_string_lossy(), &self.remote(&format!("{}.efi", self.app))])
    }

    fn download(&self, run_dir: &Path) -> Cmd {
        let [o, opt, _] = self.ssh_args();
        Cmd::new("scp", ["-q", o, opt, &self.remote("out/*"), &run_dir.to_string_lossy()])
    }

    fn deploy(&self, subcommand: &str) -> Cmd {
        let mut args: Vec<&str> = self.ssh_args().to_vec();
        args.extend(["sudo", "-n", self.config.helper.as_str(), subcommand]);
        Cmd::new("ssh", args)
    }

    fn deploy_app(&self, subcommand: &str) -> Cmd {
        let mut cmd = self.deploy(subcommand);
        cmd.args.push(self.app.to_owned());
        cmd
    }

    fn boot_id(&self) -> Cmd {
        let [o, opt, host] = self.ssh_args();
        let timeout = format!("ConnectTimeout={CONNECT_TIMEOUT_SECS}");
        Cmd::new("ssh", [o, opt, "-o", &timeout, host, "cat", BOOT_ID_PATH])
    }

    /// Remove the previous runs' files from the user-owned staging output. The stage directory
    /// is validated to be a plain relative-or-absolute path without `..`, so the glob stays
    /// inside `<stage_dir>/out/`.
    fn clear_reports(&self) -> Cmd {
        let mut args: Vec<String> = self.ssh_args().map(String::from).to_vec();
        args.extend(["rm", "-f", "--"].map(String::from));
        args.push(format!("{}/out/*", self.config.stage_dir));
        Cmd::new("ssh", args)
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

    use assert2::{assert, check};
    use rstest::rstest;

    use super::*;

    const METADATA: &str = r#"{"target_directory": "/w/target", "packages": [
        {"name": "ferrite-recon", "targets": [{"name": "dbcping", "kind": ["bin"]}, {"name": "dbcbench", "kind": ["bin"]}]}]}"#;

    const DEPLOY: &str = "ssh -o BatchMode=yes target sudo -n /usr/local/sbin/ferrite-esp";

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
    }

    impl Fake {
        fn new(answers: &[Option<&'static str>]) -> Self {
            Self {
                events: Rc::default(),
                answers: RefCell::new(answers.to_vec()),
                clock: Cell::new(Duration::ZERO),
                fail: None,
                run_exit: 0,
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

    struct FakeCapture(Events);

    impl Capture for FakeCapture {
        fn stop(self: Box<Self>) -> Option<String> {
            self.0.borrow_mut().push(Event::CaptureStop);
            None
        }
    }

    impl Env for Fake {
        fn run(&self, cmd: &Cmd, _: OutputMode) -> io::Result<CmdOutput> {
            let text = cmd.to_string();
            self.events.borrow_mut().push(Event::Run(text.clone()));
            let reply = |success: bool, code: i32, stdout: &str, stderr: &str| {
                Ok(CmdOutput { success, code: Some(code), stdout: stdout.into(), stderr: stderr.into() })
            };
            if self.fail.is_some_and(|needle| text.contains(needle)) {
                return reply(false, 1, "", "boom\n");
            }
            if text.starts_with("cargo metadata") {
                return reply(true, 0, METADATA, "");
            }
            if text.ends_with("ferrite-esp run dbcping") {
                return reply(self.run_exit == 0, self.run_exit, "", "");
            }
            if text.ends_with(BOOT_ID_PATH) {
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
            Ok(Box::new(FakeCapture(Rc::clone(&self.events))))
        }
    }

    fn plan(app: &str) -> RunPlan {
        RunPlan {
            app: app.into(),
            run_dir: "/runs/x".into(),
            timeout: Duration::from_secs(300),
            log_for: Duration::from_secs(120),
            capture: true,
        }
    }

    #[test]
    fn the_cycle_issues_exactly_these_commands_in_this_order() {
        let fake = Fake::new(&[BEFORE, BEFORE, None, None, AFTER]);
        assert!(let Ok(result) = execute(&fake, &Config::default(), &plan("dbcping")));

        let probe = "ssh -o BatchMode=yes -o ConnectTimeout=3 target cat /proc/sys/kernel/random/boot_id".to_owned();
        check!(
            fake.commands()
                == [
                    "cargo metadata --format-version 1 --no-deps".to_owned(),
                    "cargo build --target x86_64-unknown-uefi -p ferrite-recon --bin dbcping".into(),
                    "scp -q -o BatchMode=yes /w/target/x86_64-unknown-uefi/debug/dbcping.efi target:ferrite-stage/dbcping.efi"
                        .into(),
                    format!("{DEPLOY} clean-reports"),
                    format!("{DEPLOY} install dbcping"),
                    probe.clone(),
                    format!("{DEPLOY} run dbcping"),
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
            ssh_host: "laptop".into(),
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
    #[case::bench("dbcbench", CaptureKind::Bench { wait: Duration::from_secs(300) })]
    #[case::ping("dbcping", CaptureKind::Log { probe: Some("probe-from-host".into()), duration: Duration::from_secs(120) })]
    #[case::other("xhciscan", CaptureKind::Log { probe: None, duration: Duration::from_secs(120) })]
    fn the_capture_follows_the_app(#[case] app: &str, #[case] want: CaptureKind) {
        check!(capture_kind(app, Duration::from_secs(120), Duration::from_secs(300)) == want);
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
}
