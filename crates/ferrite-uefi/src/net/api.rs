//! The status page's routes, the control endpoints and the commands they carry.
//!
//! The server around them, with its `Host` and content-type guards, is `ferrite_net`'s.

use core::num::{NonZeroU64, NonZeroUsize};

use ferrite_config::parse_duration;
use ferrite_mem::suite::Preset;
use ferrite_net::http::{Rejection, Route};
use snafu::Snafu;

use crate::control::Command;
use crate::goal::Goal;
use ferrite_efi::cores::CoreChoice;

/// A rejected preset name as far as it is worth echoing back.
pub type PresetName = heapless::String<24>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    Pause,
    Resume,
    Restart,
    Cores,
    Goal,
    Report,
    Quit,
}

impl From<Control> for &'static str {
    fn from(control: Control) -> Self {
        match control {
            Control::Pause => "pause",
            Control::Resume => "resume",
            Control::Restart => "restart",
            Control::Cores => "cores",
            Control::Goal => "goal",
            Control::Report => "report",
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
    #[snafu(display("unknown field {name:?}"))]
    UnknownField { name: PresetName },
    #[snafu(display("passes must be a whole number of at least 1, or null"))]
    BadPasses,
    #[snafu(display("time must be a duration such as \"90m\", \"2h\" or \"1h30m\" (1 second to a year), or null"))]
    BadTime,
    #[snafu(display("stop_on_fault must be true, false or null"))]
    BadFlag,
}

/// The route for `method` on `path` (without the query).
pub fn resolve(method: &str, path: &str) -> Result<Route<Control>, Rejection> {
    if path == "/api/report" {
        // The one path with two methods: GET reads the report, POST writes it to the boot volume.
        return match method {
            "GET" => Ok(Route::Text),
            "POST" => Ok(Route::Control(Control::Report)),
            _ => Err(Rejection::MethodNotAllowed { allow: "GET, POST" }),
        };
    }
    let (route, allow) = match path {
        "/" | "/index.html" => (Route::Page, "GET"),
        "/api/status" => (Route::Document, "GET"),
        "/api/events" => (Route::Events, "GET"),
        "/api/pause" => (Route::Control(Control::Pause), "POST"),
        "/api/resume" => (Route::Control(Control::Resume), "POST"),
        "/api/restart" => (Route::Control(Control::Restart), "POST"),
        "/api/cores" => (Route::Control(Control::Cores), "POST"),
        "/api/goal" => (Route::Control(Control::Goal), "POST"),
        "/api/quit" => (Route::Control(Control::Quit), "POST"),
        _ => return Err(Rejection::NotFound),
    };
    if method == allow { Ok(route) } else { Err(Rejection::MethodNotAllowed { allow }) }
}

/// The command a control request's body asks for.
///
/// Pause, resume, report and quit take an empty body or any flat object; restart takes
/// `{"preset": "quick"}`, cores `{"cores": "all"}`, `{"cores": "physical"}` or `{"cores": 4}`, and goal
/// `{"passes": 3, "time": "2h", "stop_on_fault": true}` with every field optional (`{}` is no goal)
/// and no other field allowed.
pub fn command(control: Control, body: &[u8]) -> Result<Command, BodyError> {
    let body = body.trim_ascii();
    let get = |field: &'static str| -> Result<Scalar<'_>, BodyError> {
        flat_field(body, field)?.ok_or(BodyError::Missing { field })
    };
    match control {
        Control::Pause | Control::Resume | Control::Report | Control::Quit => {
            if !body.is_empty() {
                flat_field(body, "")?;
            }
            Ok(match control {
                Control::Pause => Command::Pause,
                Control::Resume => Command::Resume,
                Control::Report => Command::SaveReport,
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
        Control::Goal => goal(body).map(Command::Goal),
    }
}

/// The goal a body sets: fields that are absent or `null` are not part of it.
fn goal(body: &[u8]) -> Result<Goal, BodyError> {
    let mut goal = Goal::UNLIMITED;
    flat_scan(body, |name, value| {
        match (name, value) {
            ("passes", Scalar::Int(n)) => goal.passes = Some(NonZeroU64::new(n).ok_or(BodyError::BadPasses)?),
            ("passes", Scalar::Null) => goal.passes = None,
            ("passes", _) => return Err(BodyError::BadPasses),
            ("time", Scalar::Str(text)) => goal.time = Some(parse_duration(text).ok_or(BodyError::BadTime)?),
            ("time", Scalar::Null) => goal.time = None,
            ("time", _) => return Err(BodyError::BadTime),
            ("stop_on_fault", Scalar::Bool(on)) => goal.stop_on_fault = on,
            ("stop_on_fault", Scalar::Null) => goal.stop_on_fault = false,
            ("stop_on_fault", _) => return Err(BodyError::BadFlag),
            (other, _) => return Err(BodyError::UnknownField { name: truncated(other) }),
        }
        Ok(())
    })?;
    Ok(goal)
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
    Bool(bool),
    Null,
}

/// Parse `body` as a flat JSON object and return the value of `key`, the last one if repeated.
fn flat_field<'a>(body: &'a [u8], key: &str) -> Result<Option<Scalar<'a>>, BodyError> {
    let mut found = None;
    flat_scan(body, |name, value| {
        if name == key {
            found = Some(value);
        }
        Ok(())
    })?;
    Ok(found)
}

/// Parse `body` as a flat JSON object and hand `visit` each member in order.
fn flat_scan<'a>(
    body: &'a [u8],
    mut visit: impl FnMut(&'a str, Scalar<'a>) -> Result<(), BodyError>,
) -> Result<(), BodyError> {
    let mut c = Cursor { s: body, at: 0 };
    c.expect(b'{')?;
    if !c.eat(b'}') {
        loop {
            let name = c.string()?;
            c.expect(b':')?;
            let value = c.scalar()?;
            visit(name, value)?;
            if c.eat(b'}') {
                break;
            }
            c.expect(b',')?;
        }
    }
    c.skip_ws();
    if c.at == body.len() { Ok(()) } else { Err(c.syntax()) }
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
                Ok(match literal {
                    b"true" => Scalar::Bool(true),
                    b"false" => Scalar::Bool(false),
                    _ => Scalar::Null,
                })
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;
    use rstest::rstest;

    use super::*;

    fn goal(passes: Option<u64>, secs: Option<u64>, stop_on_fault: bool) -> Goal {
        Goal {
            passes: passes.and_then(NonZeroU64::new),
            time: secs.map(core::time::Duration::from_secs),
            stop_on_fault,
        }
    }

    #[rstest]
    #[case::page("GET", "/", Ok(Route::Page))]
    #[case::index("GET", "/index.html", Ok(Route::Page))]
    #[case::status("GET", "/api/status", Ok(Route::Document))]
    #[case::events("GET", "/api/events", Ok(Route::Events))]
    #[case::pause("POST", "/api/pause", Ok(Route::Control(Control::Pause)))]
    #[case::quit("POST", "/api/quit", Ok(Route::Control(Control::Quit)))]
    #[case::goal("POST", "/api/goal", Ok(Route::Control(Control::Goal)))]
    #[case::report_read("GET", "/api/report", Ok(Route::Text))]
    #[case::report_write("POST", "/api/report", Ok(Route::Control(Control::Report)))]
    #[case::report_other("PUT", "/api/report", Err(Rejection::MethodNotAllowed { allow: "GET, POST" }))]
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
    #[case::report_empty(Control::Report, "", Command::SaveReport)]
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
    #[case::goal_none(Control::Goal, "{}", Command::Goal(Goal::UNLIMITED))]
    #[case::goal_passes(Control::Goal, r#"{"passes":3}"#, Command::Goal(goal(Some(3), None, false)))]
    #[case::goal_all(
        Control::Goal,
        r#" { "passes" : 2 , "time" : "1h30m" , "stop_on_fault" : true } "#,
        Command::Goal(goal(Some(2), Some(5400), true))
    )]
    #[case::goal_nulls_clear(
        Control::Goal,
        r#"{"passes":null,"time":null,"stop_on_fault":null}"#,
        Command::Goal(Goal::UNLIMITED)
    )]
    #[case::goal_last_wins(Control::Goal, r#"{"passes":2,"passes":5}"#, Command::Goal(goal(Some(5), None, false)))]
    fn commands(#[case] control: Control, #[case] body: &str, #[case] expected: Command) {
        check!(command(control, body.as_bytes()) == Ok(expected));
    }

    #[rstest]
    #[case::restart_empty(Control::Restart, "", BodyError::Syntax { at: 0 })]
    #[case::restart_missing(Control::Restart, "{}", BodyError::Missing { field: "preset" })]
    #[case::restart_number(Control::Restart, r#"{"preset":1}"#, BodyError::WrongType { field: "preset" })]
    #[case::restart_unknown(Control::Restart, r#"{"preset":"fast"}"#, BodyError::UnknownPreset { name: truncated("fast") })]
    #[case::goal_empty(Control::Goal, "", BodyError::Syntax { at: 0 })]
    #[case::goal_zero_passes(Control::Goal, r#"{"passes":0}"#, BodyError::BadPasses)]
    #[case::goal_text_passes(Control::Goal, r#"{"passes":"3"}"#, BodyError::BadPasses)]
    #[case::goal_bad_time(Control::Goal, r#"{"time":"90"}"#, BodyError::BadTime)]
    #[case::goal_number_time(Control::Goal, r#"{"time":90}"#, BodyError::BadTime)]
    #[case::goal_bad_flag(Control::Goal, r#"{"stop_on_fault":"yes"}"#, BodyError::BadFlag)]
    #[case::goal_unknown_field(Control::Goal, r#"{"pases":3}"#, BodyError::UnknownField { name: truncated("pases") })]
    #[case::goal_overflow(Control::Goal, r#"{"passes":99999999999999999999}"#, BodyError::Overflow)]
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
