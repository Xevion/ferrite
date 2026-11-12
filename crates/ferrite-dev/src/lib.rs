//! Desktop tooling for the ferrite DbC link: log capture, the benchmark peer and the remote run cycle.

pub mod app;
pub mod bench;
pub mod cli;
pub mod config;
pub mod connect;
pub mod emit;
pub mod error;
pub mod live;
pub mod log;
pub mod metadata;
pub mod run;
pub mod runner;
pub mod tty;

#[cfg(test)]
mod testing;

/// An error followed by each error it wraps, joined with `: `.
pub fn chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(inner) = source {
        text.push_str(": ");
        text.push_str(&inner.to_string());
        source = inner.source();
    }
    text
}
