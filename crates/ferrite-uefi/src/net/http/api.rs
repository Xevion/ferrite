//! What a request asks for, and the commands the control endpoints carry.
//!
//! Control requests must say `Content-Type: application/json`. A plain HTML form can only send
//! form or text bodies, and a cross-origin script that sets this type has to pass a CORS preflight,
//! which the server never grants, so another site cannot drive the controls from a visitor's browser.
//!
//! Every request must also be addressed to this machine: a page on another site whose name has been
//! re-pointed at our address (DNS rebinding) would otherwise be same-origin with the API and read it
//! freely. Its requests carry its own name in `Host`, which is refused.

use core::fmt;
use core::net::Ipv6Addr;
use core::num::NonZeroUsize;

use ferrite_mem::suite::Preset;
use smoltcp::wire::Ipv4Address;
use snafu::Snafu;

use crate::control::Command;
use crate::mdns::HOST;
use crate::sweep::CoreChoice;

/// Control bodies longer than this are refused unread.
pub const BODY_CAP: usize = 256;

/// A rejected preset name as far as it is worth echoing back.
pub type PresetName = heapless::String<24>;

const PRESETS: [Preset; 4] = [Preset::Quick, Preset::Standard, Preset::Thorough, Preset::Destructive];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endpoint {
    Page,
    Status,
    Events,
    Control(Control),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    Pause,
    Resume,
    Restart,
    Cores,
    Quit,
}

impl Control {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Restart => "restart",
            Self::Cores => "cores",
            Self::Quit => "quit",
        }
    }
}

/// Why a request gets no answer but an error.
#[derive(Debug, PartialEq, Eq, Snafu)]
pub enum Rejection {
    #[snafu(display("no such resource"))]
    NotFound,
    #[snafu(display("method not allowed, use {allow}"))]
    MethodNotAllowed { allow: &'static str },
    #[snafu(display("control requests must be sent as application/json"))]
    NotJson,
    #[snafu(display("body longer than {BODY_CAP} bytes"))]
    TooLarge,
    #[snafu(display("unreadable body"))]
    Unreadable,
    #[snafu(display("bad body: {source}"))]
    BadBody { source: BodyError },
    #[snafu(display("too many event streams open, poll /api/status instead"))]
    Busy,
    #[snafu(display("missing Host header"))]
    NoHost,
    #[snafu(display("not served under that name, use {HOST} or this machine's address"))]
    Misdirected,
}

impl Rejection {
    pub const fn status(&self) -> u16 {
        match self {
            Self::NotFound => 404,
            Self::MethodNotAllowed { .. } => 405,
            Self::NotJson => 415,
            Self::TooLarge => 413,
            Self::Unreadable | Self::BadBody { .. } | Self::NoHost => 400,
            Self::Busy => 503,
            Self::Misdirected => 421,
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

/// The endpoint for `method` on `path` (without the query).
pub fn resolve(method: &str, path: &str) -> Result<Endpoint, Rejection> {
    let (endpoint, allow) = match path {
        "/" | "/index.html" => (Endpoint::Page, "GET"),
        "/api/status" => (Endpoint::Status, "GET"),
        "/api/events" => (Endpoint::Events, "GET"),
        "/api/pause" => (Endpoint::Control(Control::Pause), "POST"),
        "/api/resume" => (Endpoint::Control(Control::Resume), "POST"),
        "/api/restart" => (Endpoint::Control(Control::Restart), "POST"),
        "/api/cores" => (Endpoint::Control(Control::Cores), "POST"),
        "/api/quit" => (Endpoint::Control(Control::Quit), "POST"),
        _ => return Err(Rejection::NotFound),
    };
    if method == allow { Ok(endpoint) } else { Err(Rejection::MethodNotAllowed { allow }) }
}

/// The cross-site guard: the media type must be `application/json`, parameters allowed.
pub fn check_json(content_type: Option<&[u8]>) -> Result<(), Rejection> {
    let media = content_type.and_then(|value| value.split(|&b| b == b';').next()).map(<[u8]>::trim_ascii);
    if media.is_some_and(|m| m.eq_ignore_ascii_case(b"application/json")) { Ok(()) } else { Err(Rejection::NotJson) }
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
    PRESETS
        .into_iter()
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

/// Whether `host`, the value of a `Host` header, names this machine: its leased `address`, its mDNS
/// name or loopback (what a forwarded port arrives as), with or without a port.
pub fn check_host(host: Option<&[u8]>, address: Option<Ipv4Address>) -> Result<(), Rejection> {
    let host = host.ok_or(Rejection::NoHost)?;
    if split_port(host).is_some_and(|name| names_us(name, address)) { Ok(()) } else { Err(Rejection::Misdirected) }
}

/// The host part of `host`, when what follows it (if anything) is a port.
fn split_port(host: &[u8]) -> Option<&[u8]> {
    let (name, port) = if host.first() == Some(&b'[') {
        let (name, rest) = host.split_at(host.iter().position(|&b| b == b']')? + 1);
        (name, if rest.is_empty() { None } else { Some(rest.strip_prefix(b":")?) })
    } else {
        host.iter().rposition(|&b| b == b':').map_or((host, None), |colon| (&host[..colon], Some(&host[colon + 1..])))
    };
    let port_ok = port.is_none_or(|port| {
        port.iter().all(u8::is_ascii_digit) && core::str::from_utf8(port).is_ok_and(|p| p.parse::<u16>().is_ok())
    });
    port_ok.then_some(name)
}

fn names_us(name: &[u8], address: Option<Ipv4Address>) -> bool {
    if let Some(inner) = name.strip_prefix(b"[").and_then(|n| n.strip_suffix(b"]")) {
        return core::str::from_utf8(inner)
            .ok()
            .and_then(|s| s.parse::<Ipv6Addr>().ok())
            .is_some_and(|ip| ip.is_loopback());
    }
    if let Some(ip) = core::str::from_utf8(name).ok().and_then(|s| s.parse::<Ipv4Address>().ok()) {
        return ip.is_loopback() || Some(ip) == address;
    }
    let name = name.strip_suffix(b".").unwrap_or(name);
    name.eq_ignore_ascii_case(b"localhost") || display_matches(&HOST, name)
}

/// Whether `value` displays as `text`, ignoring ASCII case, without building the string.
fn display_matches(value: &impl fmt::Display, text: &[u8]) -> bool {
    struct Rest<'a>(&'a [u8]);

    impl fmt::Write for Rest<'_> {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            match self.0.split_at_checked(s.len()) {
                Some((head, rest)) if head.eq_ignore_ascii_case(s.as_bytes()) => {
                    self.0 = rest;
                    Ok(())
                },
                _ => Err(fmt::Error),
            }
        }
    }

    let mut rest = Rest(text);
    fmt::write(&mut rest, format_args!("{value}")).is_ok() && rest.0.is_empty()
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
    #[case::page("GET", "/", Ok(Endpoint::Page))]
    #[case::index("GET", "/index.html", Ok(Endpoint::Page))]
    #[case::status("GET", "/api/status", Ok(Endpoint::Status))]
    #[case::events("GET", "/api/events", Ok(Endpoint::Events))]
    #[case::pause("POST", "/api/pause", Ok(Endpoint::Control(Control::Pause)))]
    #[case::quit("POST", "/api/quit", Ok(Endpoint::Control(Control::Quit)))]
    #[case::get_control("GET", "/api/pause", Err(Rejection::MethodNotAllowed { allow: "POST" }))]
    #[case::post_page("POST", "/", Err(Rejection::MethodNotAllowed { allow: "GET" }))]
    #[case::preflight("OPTIONS", "/api/quit", Err(Rejection::MethodNotAllowed { allow: "POST" }))]
    #[case::unknown("GET", "/favicon.ico", Err(Rejection::NotFound))]
    #[case::trailing_slash("POST", "/api/pause/", Err(Rejection::NotFound))]
    fn routes(#[case] method: &str, #[case] path: &str, #[case] expected: Result<Endpoint, Rejection>) {
        check!(resolve(method, path) == expected);
    }

    #[rstest]
    #[case::json(Some(&b"application/json"[..]), true)]
    #[case::charset(Some(&b"Application/JSON; charset=utf-8"[..]), true)]
    #[case::form(Some(&b"application/x-www-form-urlencoded"[..]), false)]
    #[case::text(Some(&b"text/plain"[..]), false)]
    #[case::multipart(Some(&b"multipart/form-data; boundary=x"[..]), false)]
    #[case::json_suffix(Some(&b"application/json-seq"[..]), false)]
    #[case::missing(None, false)]
    fn cross_site_guard(#[case] content_type: Option<&[u8]>, #[case] allowed: bool) {
        check!(check_json(content_type).is_ok() == allowed);
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

    const LEASE: Ipv4Address = Ipv4Address::new(10, 0, 2, 15);

    #[rstest]
    #[case::address("10.0.2.15")]
    #[case::address_port("10.0.2.15:80")]
    #[case::mdns_name("ferrite.local")]
    #[case::mdns_name_shouting("FERRITE.Local:8080")]
    #[case::mdns_name_rooted("ferrite.local.")]
    #[case::loopback("127.0.0.1:8081")]
    #[case::loopback_range("127.5.5.5")]
    #[case::localhost("localhost:8081")]
    #[case::loopback_v6("[::1]:8081")]
    #[case::loopback_v6_bare("[::1]")]
    fn hosts_that_are_ours(#[case] host: &str) {
        check!(check_host(Some(host.as_bytes()), Some(LEASE)) == Ok(()));
    }

    #[rstest]
    #[case::other_address("10.0.2.16")]
    #[case::other_name("evil.example")]
    #[case::name_as_prefix("ferrite.local.evil.example")]
    #[case::name_as_suffix("notferrite.local")]
    #[case::name_with_extra("ferrite.localx")]
    #[case::address_as_prefix("10.0.2.15.evil.example")]
    #[case::userinfo("10.0.2.15@evil.example")]
    #[case::octal_address("010.0.2.15")]
    #[case::short_loopback("127.1")]
    #[case::empty("")]
    #[case::port_only(":80")]
    #[case::empty_port("10.0.2.15:")]
    #[case::port_too_big("10.0.2.15:65536")]
    #[case::port_not_digits("10.0.2.15:8o")]
    #[case::two_ports("10.0.2.15:80:80")]
    #[case::other_v6("[::2]")]
    #[case::unclosed_v6("[::1")]
    #[case::v6_junk("[::1]x")]
    fn hosts_that_are_not(#[case] host: &str) {
        check!(check_host(Some(host.as_bytes()), Some(LEASE)) == Err(Rejection::Misdirected));
    }

    #[test]
    fn without_a_lease_only_the_name_and_loopback_pass() {
        check!(check_host(Some(b"10.0.2.15"), None) == Err(Rejection::Misdirected));
        check!(check_host(Some(b"ferrite.local"), None) == Ok(()));
        check!(check_host(Some(b"127.0.0.1"), None) == Ok(()));
    }

    #[test]
    fn a_missing_host_is_a_bad_request() {
        check!(check_host(None, Some(LEASE)) == Err(Rejection::NoHost));
        check!(Rejection::NoHost.status() == 400);
        check!(Rejection::Misdirected.status() == 421);
    }
}
