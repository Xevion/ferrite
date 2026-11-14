//! Reading the file: syntax, shape and values, each section on its own, with the line and key of
//! whatever is wrong and never the value that is.

use alloc::string::ToString as _;
use alloc::vec::Vec;
use core::fmt;
use core::ops::Range;

use ferrite_net::notify;
use ferrite_wpa::env::Credentials;
use serde::de::DeserializeOwned;
use toml::Spanned;
use toml::de::{DeTable, DeValue, Error as DeError, ValueDeserializer};

use crate::convert::{self, Invalid};
use crate::error::{ConfigError, Path, Problem};
use crate::run::RunGoal;
use crate::time::TimeSettings;

/// What a settings file configures, and what in it was wrong.
///
/// A section with a problem is left out of its field and named in `errors`, and the others are
/// unaffected. A file that is not valid TOML configures nothing.
#[derive(Clone, Default)]
pub struct Loaded {
    pub credentials: Option<Credentials>,
    pub notify: Option<notify::Config>,
    pub run: Option<RunGoal>,
    pub time: Option<TimeSettings>,
    /// One per section that could not be used, in file order, or the one that stopped the whole file.
    pub errors: Vec<ConfigError>,
}

impl Loaded {
    /// The settings when the file had no problem, otherwise its first one.
    pub fn into_result(mut self) -> Result<Self, ConfigError> {
        if self.errors.is_empty() { Ok(self) } else { Err(self.errors.swap_remove(0)) }
    }
}

/// Counts only: the URL and the passphrases stay out of `{:?}`.
impl fmt::Debug for Loaded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Loaded")
            .field("networks", &self.credentials.as_ref().map(|c| c.networks().len()))
            .field("notify", &self.notify.is_some())
            .field("run", &self.run)
            .field("time", &self.time)
            .field("errors", &self.errors)
            .finish()
    }
}

/// Read a settings file.
pub fn load(bytes: &[u8]) -> Loaded {
    let mut loaded = Loaded::default();
    let text = match core::str::from_utf8(bytes) {
        Ok(text) => text.strip_prefix('\u{feff}').unwrap_or(text),
        Err(e) => {
            let valid = core::str::from_utf8(&bytes[..e.valid_up_to()]).unwrap_or_default();
            let (line, column) = position(valid, valid.len());
            loaded.errors.push(ConfigError { line, column, path: Path::default(), problem: Problem::Encoding });
            return loaded;
        },
    };
    let document = match DeTable::parse(text) {
        Ok(document) => document,
        Err(e) => {
            let (line, column) = position(text, e.span().map_or(0, |span| span.start));
            let problem = Problem::Syntax { detail: e.message().to_string() };
            loaded.errors.push(ConfigError { line, column, path: Path::default(), problem });
            return loaded;
        },
    };
    let source = Source { text, root: document.get_ref() };
    for (key, value) in document.get_ref() {
        let outcome = match key.get_ref().as_ref() {
            "wifi" => source.section(key, value, convert::credentials).map(|c| loaded.credentials = Some(c)),
            "notify" => source.section(key, value, convert::notify_config).map(|c| loaded.notify = Some(c)),
            "run" => source.section(key, value, convert::run_goal).map(|g| loaded.run = Some(g)),
            "time" => source.section(key, value, convert::time_settings).map(|t| loaded.time = Some(t)),
            name => {
                let (line, column) = position(text, key.span().start);
                Err(ConfigError { line, column, path: Path::default().key(name), problem: Problem::UnknownKey })
            },
        };
        loaded.errors.extend(outcome.err());
    }
    loaded
}

/// The document being read, to turn a byte range or a path in it into the other.
struct Source<'t, 'i> {
    text: &'t str,
    root: &'t DeTable<'i>,
}

impl Source<'_, '_> {
    /// Read the top-level `key` as the plain section `T`, then check its values.
    fn section<T: DeserializeOwned, U>(
        &self,
        key: &Spanned<alloc::borrow::Cow<'_, str>>,
        value: &Spanned<DeValue<'_>>,
        check: impl FnOnce(&T) -> Result<U, Invalid>,
    ) -> Result<U, ConfigError> {
        let plain = T::deserialize(ValueDeserializer::from(value.clone())).map_err(|e| self.shape_error(key, &e))?;
        check(&plain).map_err(|invalid| self.value_error(&invalid))
    }

    /// An error the TOML library found in the shape of the data, with its message replaced by one that
    /// cannot quote the input.
    fn shape_error(&self, section: &Spanned<alloc::borrow::Cow<'_, str>>, e: &DeError) -> ConfigError {
        let span = e.span().unwrap_or_else(|| section.span());
        let mut path = Path::default();
        if !locate(self.root, &span, &mut path) {
            path = Path::default().key(section.get_ref().as_ref());
        }
        let (line, column) = position(self.text, span.start);
        ConfigError { line, column, path, problem: classify(e.message()) }
    }

    /// An error in a value, at the value's place in the file.
    fn value_error(&self, invalid: &Invalid) -> ConfigError {
        let offset = span_of(self.root, &invalid.path).map_or(0, |span| span.start);
        let (line, column) = position(self.text, offset);
        ConfigError { line, column, path: invalid.path.clone(), problem: invalid.problem.clone() }
    }
}

/// A message of the TOML library or of serde, as a [`Problem`]. Their wording for a value of the wrong
/// kind quotes it, so only what the schema expected is kept, and anything unrecognized is dropped.
fn classify(message: &str) -> Problem {
    // The text after the last separator is the schema's, whatever the quoted value held.
    let expected = || message.rfind(", expected ").map(|at| message[at + ", expected ".len()..].to_string());
    if let Some(rest) = message.strip_prefix("missing field `") {
        return Problem::MissingKey { key: rest.split('`').next().unwrap_or_default().to_string() };
    }
    if message.starts_with("unknown field `") {
        return Problem::UnknownKey;
    }
    if message.starts_with("invalid type: ") {
        let expected = match expected().as_deref() {
            Some("a sequence") => "an array".to_string(),
            Some(e) if e.starts_with("struct ") => "a table".to_string(),
            Some(e) => e.to_string(),
            None => "another type".to_string(),
        };
        return Problem::WrongType { expected };
    }
    let a_value = message.starts_with("invalid value: ")
        || message.starts_with("unknown variant `")
        || message.starts_with("invalid length ");
    match expected() {
        Some(expected) if a_value => Problem::BadValue { expected },
        _ => Problem::Unreadable,
    }
}

/// The line and column (from 1, in characters) of byte `offset` in `text`.
fn position(text: &str, offset: usize) -> (usize, usize) {
    let mut lines = text.as_bytes()[..offset.min(text.len())].split(|&b| b == b'\n');
    let current = lines.next_back().unwrap_or_default();
    let column = 1 + current.iter().filter(|&&b| b & 0xC0 != 0x80).count();
    (1 + lines.count(), column)
}

/// The path to the key or value at exactly `span`. A table's span is its header, so containment does
/// not lead to what is inside it, and every span in the document is searched.
fn locate(table: &DeTable<'_>, span: &Range<usize>, path: &mut Path) -> bool {
    for (key, value) in table {
        path.push_key(key.get_ref());
        if key.span() == *span || locate_in(value, span, path) {
            return true;
        }
        path.pop();
    }
    false
}

fn locate_in(value: &Spanned<DeValue<'_>>, span: &Range<usize>, path: &mut Path) -> bool {
    // Children first: an array of tables starts where its first table does, and that table is meant.
    let inside = match value.get_ref() {
        DeValue::Table(table) => locate(table, span, path),
        DeValue::Array(items) => items.iter().enumerate().any(|(index, item)| {
            path.push_index(index);
            let found = locate_in(item, span, path);
            if !found {
                path.pop();
            }
            found
        }),
        _ => false,
    };
    inside || value.span() == *span
}

/// The span of the value at `path`, or of the deepest table that exists on the way to it.
fn span_of(table: &DeTable<'_>, path: &Path) -> Option<Range<usize>> {
    use crate::error::Segment;

    let mut segments = path.segments().iter();
    let Some(Segment::Key(first)) = segments.next() else { return None };
    let mut value = table.iter().find(|(key, _)| key.get_ref().as_ref() == first.as_str())?.1;
    for segment in segments {
        let next = match (segment, value.get_ref()) {
            (Segment::Key(name), DeValue::Table(table)) => {
                table.iter().find(|(key, _)| key.get_ref().as_ref() == name.as_str()).map(|(_, value)| value)
            },
            (Segment::Index(index), DeValue::Array(items)) => items.get(*index),
            _ => None,
        };
        let Some(next) = next else { break };
        value = next;
    }
    Some(value.span())
}
