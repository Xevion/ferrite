//! `push`: send a `.efi` to the resident loader, run it there and stream what it prints.
//!
//! The image is resolved like `run` resolves its app (a binary name is built, a path is used as
//! is). With `boot`, a loader that does not answer in time is started on the target first.

use std::fs;
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use ferrite_wire::link::{PushKind, ShortStr, ShortStrError};
use snafu::{ResultExt, Snafu, ensure};

use crate::config::Config;
use crate::conversation::{Conversation, ConversationError, Goal, Outcome, PushPlan, Stage, Timeouts};
use crate::efi_status::EfiStatus;
use crate::link::LinkOptions;
use crate::live::Live;
use crate::loader::LOADER_APP;
use crate::run::{self, RECOVERY, RunError, valid_app_name};
use crate::session::{self, Console, Dialer, SessionError};

#[derive(Debug, Clone)]
pub struct PushOptions {
    /// A binary name to build, or the path of a `.efi`.
    pub app: String,
    pub kind: PushKind,
    pub watchdog: Duration,
    /// How long the loader gets to answer before `push` gives up, or starts one when `boot` is set.
    pub wait: Duration,
    /// Start the loader on the target when none answers, and wait this long for it.
    pub boot: Option<Duration>,
    /// Reset the target into its default OS once the app has exited.
    pub reboot: bool,
    /// Ask the app to stop once it has run this long.
    pub stop_after: Option<Duration>,
    pub out: Option<PathBuf>,
    pub link: LinkOptions,
}

#[derive(Debug, Snafu)]
pub enum PushError {
    #[snafu(display("`{app}` is neither a valid app name nor the path of a .efi file"))]
    BadApp { app: String },

    #[snafu(display("the watchdog must be between 1 second and {} seconds", u32::MAX))]
    BadWatchdog,

    #[snafu(display("cannot build `{app}`"))]
    Build { app: String, source: RunError },

    #[snafu(display("cannot read {}", path.display()))]
    ReadImage { path: PathBuf, source: io::Error },

    #[snafu(display("{} is empty", path.display()))]
    EmptyImage { path: PathBuf },

    #[snafu(display("the file name {name:?} cannot be sent"))]
    ImageName { name: String, source: ShortStrError },

    #[snafu(display("cannot create {}", path.display()))]
    Output { path: PathBuf, source: io::Error },

    #[snafu(display(
        "no loader answered within {}s; start one with `ferrite-dev loader boot`, or pass --boot to do it here",
        wait.as_secs()
    ))]
    NoLoader { wait: Duration },

    #[snafu(display("the loader did not answer within {}s of starting it on the target", wait.as_secs()))]
    BootTimeout { wait: Duration },

    #[snafu(display("cannot start the loader on the target"))]
    Boot { source: RunError },

    #[snafu(transparent)]
    Session { source: SessionError },

    #[snafu(display("the app exited with {status} after {:.1}s", elapsed.as_secs_f32()))]
    AppFailed { status: EfiStatus, elapsed: Duration },
}

/// What `app` names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Path(PathBuf),
    Bin(String),
}

/// A path has a directory part or a `.efi` extension, which no binary name can contain.
pub fn classify(app: &str) -> Result<Source, PushError> {
    let is_path = app.contains('/') || Path::new(app).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("efi"));
    if is_path {
        Ok(Source::Path(PathBuf::from(app)))
    } else if valid_app_name(app) {
        Ok(Source::Bin(app.to_owned()))
    } else {
        BadAppSnafu { app }.fail()
    }
}

/// Whole seconds for the wire, rounded up so a fraction never becomes 0.
fn watchdog_secs(watchdog: Duration) -> Result<u32, PushError> {
    let secs = watchdog.as_secs() + u64::from(watchdog.subsec_nanos() > 0);
    u32::try_from(secs).ok().filter(|&s| s > 0).ok_or(PushError::BadWatchdog)
}

fn load_plan(path: &Path, kind: PushKind, watchdog_secs: u32) -> Result<PushPlan, PushError> {
    let image = fs::read(path).context(ReadImageSnafu { path })?;
    ensure!(!image.is_empty(), EmptyImageSnafu { path });
    let name = path.file_name().map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let name = ShortStr::new(&name).context(ImageNameSnafu { name })?;
    Ok(PushPlan::new(kind, name, image, watchdog_secs))
}

/// Lines typed on stdin, each with its newline, for an app that reads input.
fn forward_stdin() -> Option<Receiver<Vec<u8>>> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("stdin".into())
        .spawn(move || {
            let mut stdin = io::stdin().lock();
            let mut line = Vec::new();
            loop {
                line.clear();
                match stdin.read_until(b'\n', &mut line) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {
                        if tx.send(line.clone()).is_err() {
                            return;
                        }
                    },
                }
            }
        })
        .ok()
        .map(|_| rx)
}

pub fn run(config: &Config, options: &PushOptions, stop: &AtomicBool) -> Result<(), PushError> {
    let secs = watchdog_secs(options.watchdog)?;
    let env = Live::new(config);
    let path = match classify(&options.app)? {
        Source::Path(path) => path,
        Source::Bin(app) => run::build_app(&env, &app).context(BuildSnafu { app })?,
    };
    let plan = load_plan(&path, options.kind, secs)?;
    let mut console = Console::stdio(options.out.as_deref())
        .context(OutputSnafu { path: options.out.clone().unwrap_or_default() })?;
    console.emitter.line(&format!(
        "--- pushing {} as {:?}: {} bytes, crc32 {:08x}, watchdog {secs}s ---",
        plan.begin.name, options.kind, plan.begin.size, plan.begin.crc32
    ));

    let mut machine = Conversation::new(Goal::Push(Box::new(plan)), Timeouts::with_ready(options.wait), Instant::now());
    if let Some(after) = options.stop_after {
        machine = machine.with_stop_after(after);
    }
    let mut dialer = Dialer::new(config, options.link);
    let input = forward_stdin();
    let mut conversed = session::converse(&mut machine, &mut dialer, &mut console, input.as_ref(), stop);

    if let (Some(boot_wait), Err(SessionError::Conversation { source })) = (options.boot, &conversed)
        && matches!(source, ConversationError::Timeout { waiting_for: Stage::Greeting, .. })
    {
        console
            .emitter
            .line(&format!("--- no loader answered in {}s; starting one on the target ---", options.wait.as_secs()));
        run::stage_and_reboot(&env, config, LOADER_APP).context(BootSnafu)?;
        if machine.restart_greeting(Instant::now(), boot_wait) {
            conversed = session::converse(&mut machine, &mut dialer, &mut console, input.as_ref(), stop);
        }
    }

    let (outcome, mut link) = conversed.map_err(|error| match (error, options.boot) {
        (
            SessionError::Conversation { source: ConversationError::Timeout { waiting_for: Stage::Greeting, .. } },
            None,
        ) => PushError::NoLoader { wait: options.wait },
        (
            SessionError::Conversation { source: ConversationError::Timeout { waiting_for: Stage::Greeting, .. } },
            Some(wait),
        ) => {
            console.emitter.plain(RECOVERY);
            PushError::BootTimeout { wait }
        },
        (other, _) => PushError::Session { source: other },
    })?;

    match outcome {
        Outcome::Exited(exited) => {
            let status = EfiStatus::from_code(exited.status);
            console.emitter.line(&format!("--- exited: {status} after {:.3}s ---", exited.elapsed().as_secs_f32()));
            if options.reboot {
                session::reboot(&mut link, &dialer, &mut console, stop)?;
            }
            ensure!(status.is_success(), AppFailedSnafu { status, elapsed: exited.elapsed() });
        },
        Outcome::Replaced(info) => {
            console.emitter.line(&format!("--- loader replaced: {} (protocol {}) ---", info.build, info.proto));
        },
        Outcome::Ready(_) => {},
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::bin("dbcping", Some(Source::Bin("dbcping".into())))]
    #[case::loader("ferrite-loader", Some(Source::Bin("ferrite-loader".into())))]
    #[case::relative_path("target/x86_64-unknown-uefi/debug/dbcping.efi", Some(Source::Path("target/x86_64-unknown-uefi/debug/dbcping.efi".into())))]
    #[case::bare_efi("app.efi", Some(Source::Path("app.efi".into())))]
    #[case::shouty_efi("APP.EFI", Some(Source::Path("APP.EFI".into())))]
    #[case::dot_slash("./dbcping", Some(Source::Path("./dbcping".into())))]
    #[case::bad_name("Ping Me", None)]
    #[case::injection("x; reboot", None)]
    fn an_app_is_a_binary_name_or_a_path(#[case] app: &str, #[case] want: Option<Source>) {
        check!(classify(app).ok() == want);
    }

    #[rstest]
    #[case::default(Duration::from_secs(300), Some(300))]
    #[case::fraction_rounds_up(Duration::from_millis(1500), Some(2))]
    #[case::tiny_is_one_second(Duration::from_millis(1), Some(1))]
    #[case::zero(Duration::ZERO, None)]
    #[case::too_long(Duration::from_secs(u64::from(u32::MAX) + 1), None)]
    fn the_watchdog_is_whole_seconds_and_never_zero(#[case] given: Duration, #[case] want: Option<u32>) {
        check!(watchdog_secs(given).ok() == want);
    }

    #[test]
    fn an_empty_image_and_a_long_name_are_refused_before_connecting() {
        let dir = std::env::temp_dir().join(format!("ferrite-dev-push-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("scratch dir");
        let empty = dir.join("empty.efi");
        fs::write(&empty, b"").expect("empty file");
        assert!(let Err(PushError::EmptyImage { .. }) = load_plan(&empty, PushKind::App, 60));

        let long = dir.join(format!("{}.efi", "n".repeat(70)));
        fs::write(&long, b"MZ").expect("long-named file");
        assert!(let Err(PushError::ImageName { .. }) = load_plan(&long, PushKind::App, 60));

        let fine = dir.join("fine.efi");
        fs::write(&fine, b"MZ").expect("file");
        assert!(let Ok(plan) = load_plan(&fine, PushKind::Loader, 60));
        check!((plan.begin.name.as_str(), plan.begin.size, plan.begin.kind) == ("fine.efi", 2, PushKind::Loader));
        fs::remove_dir_all(&dir).expect("cleanup");
    }
}
