//! Where a settings file is wrong and what is wrong with it, never what the value was.
//!
//! A value in the file can be a passphrase or a topic URL, so no error carries one, and no message is
//! taken from the TOML library when it could quote one.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use ferrite_net::notify;
use snafu::Snafu;

/// One step into the document: a key, or the position in an array of tables.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Segment {
    Key(String),
    Index(usize),
}

/// Where in the document a problem is, as `wifi.networks[1].passphrase`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Path(Vec<Segment>);

impl Path {
    #[must_use]
    pub fn key(mut self, key: impl Into<String>) -> Self {
        self.0.push(Segment::Key(key.into()));
        self
    }

    #[must_use]
    pub fn index(mut self, index: usize) -> Self {
        self.0.push(Segment::Index(index));
        self
    }

    pub fn segments(&self) -> &[Segment] {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The top-level key the path starts with: `wifi`, `notify`, `run` or `time`.
    pub fn section(&self) -> Option<&str> {
        match self.0.first() {
            Some(Segment::Key(key)) => Some(key),
            _ => None,
        }
    }

    pub(crate) fn push_key(&mut self, key: &str) {
        self.0.push(Segment::Key(key.into()));
    }

    pub(crate) fn push_index(&mut self, index: usize) {
        self.0.push(Segment::Index(index));
    }

    pub(crate) fn pop(&mut self) {
        self.0.pop();
    }
}

impl fmt::Display for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, segment) in self.0.iter().enumerate() {
            match segment {
                Segment::Key(key) if i == 0 => f.write_str(key)?,
                Segment::Key(key) => write!(f, ".{key}")?,
                Segment::Index(index) => write!(f, "[{index}]")?,
            }
        }
        Ok(())
    }
}

/// What is wrong at a place in the file. Every message is fixed text or names the schema; none holds a
/// value from the file.
#[derive(Clone, Debug, PartialEq, Eq, Snafu)]
pub enum Problem {
    #[snafu(display("not valid UTF-8"))]
    Encoding,
    #[snafu(display("not valid TOML: {detail}"))]
    Syntax { detail: String },
    #[snafu(display("unknown key"))]
    UnknownKey,
    #[snafu(display("missing key `{key}`"))]
    MissingKey { key: String },
    #[snafu(display("wrong type, expected {expected}"))]
    WrongType { expected: String },
    #[snafu(display("not an accepted value, expected {expected}"))]
    BadValue { expected: String },
    #[snafu(display("not an accepted value"))]
    Unreadable,
    #[snafu(display("an SSID is 1 to 32 bytes"))]
    Ssid,
    #[snafu(display("a passphrase is 8 to 63 printable ASCII characters"))]
    Passphrase,
    #[snafu(display("this network needs a passphrase, or security = \"open\""))]
    PassphraseMissing,
    #[snafu(display("an open network takes no passphrase"))]
    OpenWithPassphrase,
    #[snafu(display("there is no [[wifi.networks]] table"))]
    NoNetworks,
    #[snafu(display("at most {max} networks"))]
    TooManyNetworks { max: usize },
    #[snafu(display("{problem}"))]
    Url { problem: notify::Problem },
    #[snafu(display("no event listed; leave `events` out to hear about all of them"))]
    NoEvents,
    #[snafu(display("expected a time such as 90m, 2h or 1h30m, between 1 second and a year"))]
    Time,
    #[snafu(display("expected a whole number of passes, at least 1"))]
    Passes,
    #[snafu(display("not an NTP server: expected an IPv4 address or a host name"))]
    NtpServer,
    #[snafu(display("at most {max} NTP servers"))]
    TooManyNtpServers { max: usize },
    #[snafu(display("not a time zone of the tz database, expected a name such as America/Chicago or UTC"))]
    Zone,
    #[snafu(display("not a temperature limit: expected 40 to 105"))]
    TempLimit,
    #[snafu(display("not a tolerance: expected 0 to 50"))]
    Tolerance,
}

/// A problem with its place in the file, as the line and column a text editor shows (both from 1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError {
    pub line: usize,
    pub column: usize,
    /// The key the problem is at; empty for a problem with the file as a whole.
    pub path: Path,
    pub problem: Problem,
}

impl ConfigError {
    /// The section the problem is in, if it is in one.
    pub fn section(&self) -> Option<&str> {
        self.path.section()
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}, column {}: ", self.line, self.column)?;
        if !self.path.is_empty() {
            write!(f, "{}: ", self.path)?;
        }
        write!(f, "{}", self.problem)
    }
}

impl core::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use assert2::check;
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case(Path::default().key("notify").key("url"), "notify.url")]
    #[case(Path::default().key("wifi").key("networks").index(1).key("ssid"), "wifi.networks[1].ssid")]
    #[case(Path::default(), "")]
    fn paths_read_like_the_file(#[case] path: Path, #[case] text: &str) {
        check!(alloc::format!("{path}") == text);
    }
}
