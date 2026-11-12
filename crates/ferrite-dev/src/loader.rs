//! `loader`: starting and stopping the resident loader on the target.
//!
//! `boot` stages `ferrite-loader` the way `run` stages an app, but does not wait for the target to come
//! back to Linux: the loader keeps the machine, so success is the DbC device appearing and the
//! loader answering `Hello`. `reboot` asks a running loader to reset the machine, and succeeds when
//! the DbC device leaves the bus, which is the target resetting into its default OS.

use std::io;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use clap::ValueEnum;
use snafu::{ResultExt, Snafu};

use crate::config::Config;
use crate::conversation::{Conversation, ConversationError, Goal, Stage, Timeouts};
use crate::link::LinkOptions;
use crate::live::Live;
use crate::run::{self, RECOVERY, RunError};
use crate::session::{self, Console, Dialer, SessionError};

pub const LOADER_APP: &str = "ferrite-loader";

/// How long the target gets to shut down, run POST and bring the DbC up in the loader.
pub const BOOT_TIMEOUT: Duration = Duration::from_secs(180);
/// How long a loader that should already be running gets to answer before `reboot` gives up.
pub const REBOOT_WAIT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LoaderAction {
    /// Stage ferrite-loader on the target, reboot into it once and wait for it to answer on the DbC
    Boot,
    /// Have the running loader reset the target into its default OS and wait for the DbC to disappear
    Reboot,
}

impl LoaderAction {
    pub const fn default_timeout(self) -> Duration {
        match self {
            Self::Boot => BOOT_TIMEOUT,
            Self::Reboot => REBOOT_WAIT,
        }
    }
}

#[derive(Debug, Snafu)]
pub enum LoaderError {
    #[snafu(display("cannot start the loader on the target"))]
    Stage { source: RunError },

    #[snafu(display("no loader answered within {}s of starting it on the target", after.as_secs()))]
    BootTimeout { after: Duration },

    #[snafu(display("no loader answered within {}s, so there is nothing to reboot", after.as_secs()))]
    NotRunning { after: Duration },

    #[snafu(display("cannot create the output"))]
    Output { source: io::Error },

    #[snafu(transparent)]
    Session { source: SessionError },
}

#[derive(Debug, Clone)]
pub struct LoaderOptions {
    pub action: LoaderAction,
    pub timeout: Duration,
    pub link: LinkOptions,
}

pub fn run(config: &Config, options: &LoaderOptions, stop: &AtomicBool) -> Result<(), LoaderError> {
    let mut console = Console::stdio(None).context(OutputSnafu)?;
    let mut dialer = Dialer::new(config, options.link);
    let handshake = || Conversation::new(Goal::Handshake, Timeouts::with_ready(options.timeout), Instant::now());
    match options.action {
        LoaderAction::Boot => {
            run::stage_and_reboot(&Live::new(config), config, LOADER_APP).context(StageSnafu)?;
            let mut machine = handshake();
            session::converse(&mut machine, &mut dialer, &mut console, None, stop).map(|_| ()).map_err(|error| {
                match error {
                    SessionError::Conversation {
                        source: ConversationError::Timeout { waiting_for: Stage::Greeting, .. },
                    } => {
                        console.emitter.plain(RECOVERY);
                        LoaderError::BootTimeout { after: options.timeout }
                    },
                    other => LoaderError::Session { source: other },
                }
            })
        },
        LoaderAction::Reboot => {
            let mut machine = handshake();
            let (_, mut link) = session::converse(&mut machine, &mut dialer, &mut console, None, stop).map_err(
                |error| match error {
                    SessionError::Conversation {
                        source: ConversationError::Timeout { waiting_for: Stage::Greeting, .. },
                    } => LoaderError::NotRunning { after: options.timeout },
                    other => LoaderError::Session { source: other },
                },
            )?;
            session::reboot(&mut link, &dialer, &mut console, stop).map_err(Into::into)
        },
    }
}
