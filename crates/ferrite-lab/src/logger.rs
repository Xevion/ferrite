//! A `log` sink writing timestamp-free lines to stderr.

use std::io::Write as _;

use log::{LevelFilter, Log, Metadata, Record};

struct Stderr;

impl Log for Stderr {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &Record<'_>) {
        // A failed write to stderr has nowhere left to be reported.
        let _ = writeln!(std::io::stderr().lock(), "{:<5} {}", record.level(), record.args());
    }

    fn flush(&self) {}
}

static LOGGER: Stderr = Stderr;

/// Install the stderr logger at `level`.
pub fn init(level: LevelFilter) {
    // A second install is harmless: the first logger stays.
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(level);
}
