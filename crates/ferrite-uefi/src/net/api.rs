//! The status page's routes, the control endpoints and the commands they carry.
//!
//! The server around them, with its `Host` and content-type guards, is `ferrite_net`'s.

use core::num::NonZeroUsize;

use ferrite_mem::suite::Preset;
use ferrite_net::http::{Rejection, Route};
use snafu::Snafu;

use crate::control::Command;
use crate::sweep::CoreChoice;

/// A rejected preset name as far as it is worth echoing back.
pub type PresetName = heapless::String<24>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    Pause,
    Resume,
    Restart,
    Cores,
    Quit,
}

impl From<Control> for &'static str {
    fn from(control: Control) -> Self {
        match control {
            Control::Pause => "pause",
            Control::Resume => "resume",
            Control::Restart => "restart",
            Control::Cores => "cores",
            Control::Quit => "quit",
        }
    }
}

/// What is wrong with a control body.
#[derive(Debug, PartialEq, Eq, Snafu)]
pub enum BodyError {
    #[snafu(display("not a flat JSON object, at byte {at}"))]
    Syntax { at: usize },
    #[snafu(display("escapes in strings are not supported"))]
    Escape,
    #[snafu(display("number out of range"))]
    Overflow,
    #[snafu(display("missing field {field:?}"))]
    Missing { field: &'static str },
    #[snafu(display("field {field:?} has the wrong type"))]
    WrongType { field: &'static str },
    #[snafu(display("unknown preset {name:?}"))]
    UnknownPreset { name: PresetName },
    #[snafu(display("cores must be \"all\", \"physical\" or a count of at least 1"))]
    BadCores,
}

/// The route for `method` on `path` (without the query).
pub fn resolve(method: &str, path: &str) -> Result<Route<Control>, Rejection> {
    let (route, allow) = match path {
        "/" | "/index.html" => (Route::Page, "GET"),
        "/api/status" => (Route::Document, "GET"),
        "/api/events" => (Route::Events, "GET"),
        "/api/pause" => (Route::Control(Control::Pause), "POST"),
        "/api/resume" => (Route::Control(Control::Resume), "POST"),
        "/api/restart" => (Route::Control(Control::Restart), "POST"),
        "/api/cores" => (Route::Control(Control::Cores), "POST"),
        "/api/quit" => (Route::Control(Control::Quit), "POST"),
        _ => return Err(Rejection::NotFound),
    };
    if method == allow { Ok(route) } else { Err(Rejection::MethodNotAllowed { allow }) }
}

/// The command a control request's body asks for.
///
/// Pause, resume and quit take an empty body or any flat object; restart takes
/// `{"preset": "quick"}`, cores `{"cores": "all"}`, `{"cores": "physical"}` or `{"cores": 4}`.
pub fn command(control: Control, body: &[u8]) -> Result<Command, BodyError> {
    let body = body.trim_ascii();
    let get = |field: &'static str| -> Result<Scalar<'_>, BodyError> {
        flat_field(body, field)?.ok_or(BodyError::Missing { field })
    };
    match control {
        Control::Pause | Control::Resume | Control::Quit => {
            if !body.is_empty() {
                flat_field(body, "")?;
            }
            Ok(match control {
                Control::Pause => Command::Pause,
                Control::Resume => Command::Resume,
                _ => Command::Quit,
            })
        },
        Control::Restart => match get("preset")? {
            Scalar::Str(name) => preset(name).map(Command::Restart),
            _ => Err(BodyError::WrongType { field: "preset" }),
        },
        Control::Cores => {
            let choice = match get("cores")? {
                Scalar::Str("all") => CoreChoice::All,
                Scalar::Str("physical") => CoreChoice::Physical,
                Scalar::Int(n) => usize::try_from(n)
                    .ok()
                    .and_then(NonZeroUsize::new)
                    .map(CoreChoice::Count)
                    .ok_or(BodyError::BadCores)?,
                _ => return Err(BodyError::BadCores),
            };
            Ok(Command::Cores(choice))
        },
    }
}

/// The preset whose kebab-case name is `name`.
pub fn preset(name: &str) -> Result<Preset, BodyError> {
    Preset::ALL
        .iter()
        .copied()
        .find(|&p| <&str>::from(p) == name)
        .ok_or_else(|| BodyError::UnknownPreset { name: truncated(name) })
}

/// The longest prefix of `name` that fits a [`PresetName`].
fn truncated(name: &str) -> PresetName {
    let mut kept = PresetName::new();
    for c in name.chars() {
        if kept.push(c).is_err() {
            break;
        }
    }
    kept
}

/// A value in a flat object; nested values are a syntax error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scalar<'a> {
    Str(&'a str),
    Int(u64),
    /// `true`, `false` or `null`.
    Literal,
}

/// Parse `body` as a flat JSON object and return the value of `key`, the last one if repeated.
fn flat_field<'a>(body: &'a [u8], key: &str) -> Result<Option<Scalar<'a>>, BodyError> {
    let mut c = Cursor { s: body, at: 0 };
    let mut found = None;
    c.expect(b'{')?;
    if !c.eat(b'}') {
        loop {
            let name = c.string()?;
            c.expect(b':')?;
            let value = c.scalar()?;
            if name == key {
                found = Some(value);
            }
            if c.eat(b'}') {
                break;
            }
            c.expect(b',')?;
        }
    }
    c.skip_ws();
    if c.at == body.len() { Ok(found) } else { Err(c.syntax()) }
}

struct Cursor<'a> {
    s: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn syntax(&self) -> BodyError {
        BodyError::Syntax { at: self.at }
    }

    fn skip_ws(&mut self) {
        while self.s.get(self.at).is_some_and(u8::is_ascii_whitespace) {
            self.at += 1;
        }
    }

    fn eat(&mut self, b: u8) -> bool {
        self.skip_ws();
        let hit = self.s.get(self.at) == Some(&b);
        if hit {
            self.at += 1;
        }
        hit
    }

    fn expect(&mut self, b: u8) -> Result<(), BodyError> {
        if self.eat(b) { Ok(()) } else { Err(self.syntax()) }
    }

    fn string(&mut self) -> Result<&'a str, BodyError> {
        self.expect(b'"')?;
        let start = self.at;
        loop {
            match self.s.get(self.at) {
                Some(b'"') => break,
                Some(b'\\') => return Err(BodyError::Escape),
                Some(&b) if b >= 0x20 => self.at += 1,
                _ => return Err(self.syntax()),
            }
        }
        let text = core::str::from_utf8(&self.s[start..self.at]).map_err(|_| BodyError::Syntax { at: start })?;
        self.at += 1;
        Ok(text)
    }

    fn scalar(&mut self) -> Result<Scalar<'a>, BodyError> {
        self.skip_ws();
        let rest = &self.s[self.at..];
        match rest.first() {
            Some(b'"') => self.string().map(Scalar::Str),
            Some(b'0'..=b'9') => {
                let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
                let n = rest[..digits]
                    .iter()
                    .try_fold(0_u64, |n, &d| n.checked_mul(10)?.checked_add(u64::from(d - b'0')))
                    .ok_or(BodyError::Overflow)?;
                self.at += digits;
                Ok(Scalar::Int(n))
            },
            _ => {
                let literal = [&b"true"[..], b"false", b"null"].into_iter().find(|l| rest.starts_with(l));
                let literal = literal.ok_or_else(|| self.syntax())?;
                self.at += literal.len();
                Ok(Scalar::Literal)
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::page("GET", "/", Ok(Route::Page))]
    #[case::index("GET", "/index.html", Ok(Route::Page))]
    #[case::status("GET", "/api/status", Ok(Route::Document))]
    #[case::events("GET", "/api/events", Ok(Route::Events))]
    #[case::pause("POST", "/api/pause", Ok(Route::Control(Control::Pause)))]
    #[case::quit("POST", "/api/quit", Ok(Route::Control(Control::Quit)))]
    #[case::get_control("GET", "/api/pause", Err(Rejection::MethodNotAllowed { allow: "POST" }))]
    #[case::post_page("POST", "/", Err(Rejection::MethodNotAllowed { allow: "GET" }))]
    #[case::preflight("OPTIONS", "/api/quit", Err(Rejection::MethodNotAllowed { allow: "POST" }))]
    #[case::unknown("GET", "/favicon.ico", Err(Rejection::NotFound))]
    #[case::trailing_slash("POST", "/api/pause/", Err(Rejection::NotFound))]
    fn routes(#[case] method: &str, #[case] path: &str, #[case] expected: Result<Route<Control>, Rejection>) {
        check!(resolve(method, path) == expected);
    }

    #[rstest]
    #[case::pause_empty(Control::Pause, "", Command::Pause)]
    #[case::resume_object(Control::Resume, "{}", Command::Resume)]
    #[case::quit_ignores_fields(Control::Quit, r#"{"confirm": true}"#, Command::Quit)]
    #[case::restart(Control::Restart, r#"{"preset":"quick"}"#, Command::Restart(Preset::Quick))]
    #[case::restart_spaced(
        Control::Restart,
        " { \"x\" : null , \"preset\" : \"thorough\" } ",
        Command::Restart(Preset::Thorough)
    )]
    #[case::restart_memtest86_plus(
        Control::Restart,
        r#"{"preset":"memtest86-plus"}"#,
        Command::Restart(Preset::Memtest86Plus)
    )]
    #[case::cores_all(Control::Cores, r#"{"cores":"all"}"#, Command::Cores(CoreChoice::All))]
    #[case::cores_physical(Control::Cores, r#"{"cores":"physical"}"#, Command::Cores(CoreChoice::Physical))]
    #[case::cores_count(Control::Cores, r#"{"cores":3}"#, Command::Cores(CoreChoice::Count(NonZeroUsize::new(3).unwrap())))]
    fn commands(#[case] control: Control, #[case] body: &str, #[case] expected: Command) {
        check!(command(control, body.as_bytes()) == Ok(expected));
    }

    #[rstest]
    #[case::restart_empty(Control::Restart, "", BodyError::Syntax { at: 0 })]
    #[case::restart_missing(Control::Restart, "{}", BodyError::Missing { field: "preset" })]
    #[case::restart_number(Control::Restart, r#"{"preset":1}"#, BodyError::WrongType { field: "preset" })]
    #[case::restart_unknown(Control::Restart, r#"{"preset":"fast"}"#, BodyError::UnknownPreset { name: truncated("fast") })]
    #[case::cores_zero(Control::Cores, r#"{"cores":0}"#, BodyError::BadCores)]
    #[case::cores_word(Control::Cores, r#"{"cores":"half"}"#, BodyError::BadCores)]
    #[case::cores_overflow(Control::Cores, r#"{"cores":99999999999999999999}"#, BodyError::Overflow)]
    #[case::nested(Control::Cores, r#"{"cores":[1]}"#, BodyError::Syntax { at: 9 })]
    #[case::escape(Control::Restart, "{\"preset\":\"qu\\u0069ck\"}", BodyError::Escape)]
    #[case::trailing(Control::Pause, "{}x", BodyError::Syntax { at: 2 })]
    #[case::unclosed(Control::Pause, r#"{"a":1"#, BodyError::Syntax { at: 6 })]
    #[case::form_body(Control::Restart, "preset=quick", BodyError::Syntax { at: 0 })]
    fn bad_bodies(#[case] control: Control, #[case] body: &str, #[case] expected: BodyError) {
        check!(command(control, body.as_bytes()) == Err(expected));
    }
}
