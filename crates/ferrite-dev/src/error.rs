//! The error a command reports to `main`.

use std::io;
use std::path::PathBuf;

use snafu::Snafu;

use crate::bench::BenchError;
use crate::config::ConfigError;
use crate::loader::LoaderError;
use crate::log::LogError;
use crate::push::PushError;
use crate::run::RunError;

#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum Error {
    #[snafu(transparent)]
    Config { source: ConfigError },

    #[snafu(display("log capture failed"))]
    Log { source: LogError },

    #[snafu(display("benchmark failed"))]
    Bench { source: BenchError },

    #[snafu(display("run failed"))]
    Run { source: RunError },

    #[snafu(display("push failed"))]
    Push { source: PushError },

    #[snafu(display("loader command failed"))]
    Loader { source: LoaderError },

    #[snafu(display("cannot create {}", path.display()))]
    Output { path: PathBuf, source: io::Error },
}
