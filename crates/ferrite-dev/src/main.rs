use std::process::ExitCode;

use clap::Parser;
use ferrite_dev::cli::Cli;
use ferrite_dev::{app, chain};

fn main() -> ExitCode {
    match app::dispatch(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ferrite-dev: {}", chain(&e));
            ExitCode::FAILURE
        },
    }
}
