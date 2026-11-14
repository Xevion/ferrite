//! Where reports are kept: `\EFI\ferrite\reports\` on the boot volume, one file per run.
//!
//! A run's file is named when the run's first report is saved and keeps its name from then on, so
//! every later report of the run replaces it. The name is the run's start by the wall clock when the
//! clock is synced (`2026-11-14_0523.txt`), else the next free `run-NNNN.txt`; a name already taken
//! by another run gets a `-2`, `-3` and so on, so no report overwrites another run's.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use ferrite_efi::bootlog;
use ferrite_efi::volume::{self, ReadError};
use ferrite_hal::Instant;
use jiff::civil::DateTime;
use snafu::{ResultExt as _, Snafu};

/// The reports' directory on the boot volume.
pub const DIR: &str = "\\EFI\\ferrite\\reports";

const EXTENSION: &str = ".txt";
const COUNTER_PREFIX: &str = "run-";

#[derive(Debug, Snafu)]
pub enum SaveError {
    #[snafu(display("cannot list {DIR}: {source}"))]
    List { source: ReadError },
    #[snafu(display("cannot write {path}: {source}"))]
    Write { path: String, source: uefi::Error },
}

/// The directory the reports are written to.
pub trait Volume {
    /// The names of the files in it; none when it does not exist yet.
    fn names(&mut self) -> Result<Vec<String>, ReadError>;

    /// Replace the file `name` in it with `bytes`, creating the directory and the file as needed.
    fn write(&mut self, name: &str, bytes: &[u8]) -> Result<(), uefi::Error>;
}

/// [`DIR`] on the boot volume.
pub struct ReportsDir;

impl Volume for ReportsDir {
    fn names(&mut self) -> Result<Vec<String>, ReadError> {
        match volume::list_files(DIR) {
            Err(ReadError::NotFound { .. }) => Ok(Vec::new()),
            names => names,
        }
    }

    fn write(&mut self, name: &str, bytes: &[u8]) -> Result<(), uefi::Error> {
        bootlog::replace_file(&format!("{DIR}\\{name}"), bytes)
    }
}

/// A report that was written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Saved {
    pub path: String,
    pub bytes: usize,
}

/// A run's file: which run it belongs to, and its name.
struct Current {
    run: Option<Instant>,
    name: String,
}

/// Writes each run's report to its own file.
pub struct Saver {
    volume: Box<dyn Volume>,
    current: Option<Current>,
}

impl Saver {
    #[must_use]
    pub fn new(volume: Box<dyn Volume>) -> Self {
        Self { volume, current: None }
    }

    /// Write `text` as the report of the run that began at `run` (`None` before its first test). A
    /// run the saver has not written before gets a new file, named from `stamp` (see [`file_stamp`])
    /// when the clock has one; a later report of the same run replaces its file. Nothing is
    /// remembered of a write that fails, so the next report tries the whole thing again.
    pub fn save(&mut self, run: Option<Instant>, stamp: Option<&str>, text: &str) -> Result<Saved, SaveError> {
        let name = match &self.current {
            Some(current) if current.run == run => current.name.clone(),
            _ => choose_name(stamp, &self.volume.names().context(ListSnafu)?),
        };
        let path = format!("{DIR}\\{name}");
        self.volume.write(&name, text.as_bytes()).with_context(|_| WriteSnafu { path: path.clone() })?;
        self.current = Some(Current { run, name });
        Ok(Saved { path, bytes: text.len() })
    }
}

/// `2026-11-14_0523`: a time as it is used in a file name, to the minute.
#[must_use]
pub fn file_stamp(time: DateTime) -> String {
    format!("{:04}-{:02}-{:02}_{:02}{:02}", time.year(), time.month(), time.day(), time.hour(), time.minute())
}

/// The name for a new run's file among the `taken` ones: the stamp's, made unique, or the next
/// `run-NNNN.txt` after every one there is. The volume is FAT, which does not tell names apart by case.
fn choose_name(stamp: Option<&str>, taken: &[String]) -> String {
    let is_taken = |name: &str| taken.iter().any(|t| t.eq_ignore_ascii_case(name));
    let Some(stamp) = stamp else {
        let last = taken.iter().filter_map(|t| counter_of(t)).max().unwrap_or(0);
        return format!("{COUNTER_PREFIX}{:04}{EXTENSION}", last.saturating_add(1));
    };
    let plain = format!("{stamp}{EXTENSION}");
    if !is_taken(&plain) {
        return plain;
    }
    (2..=u32::MAX).map(|n| format!("{stamp}-{n}{EXTENSION}")).find(|name| !is_taken(name)).unwrap_or(plain)
}

/// The number in a `run-NNNN.txt` name.
fn counter_of(name: &str) -> Option<u32> {
    let lower = name.to_ascii_lowercase();
    lower.strip_prefix(COUNTER_PREFIX)?.strip_suffix(EXTENSION)?.parse().ok()
}

#[cfg(test)]
mod tests;
