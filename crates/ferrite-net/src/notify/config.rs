//! The notification settings, read from `.env` lines: where to POST, in what body format, for which
//! events.
//!
//! Errors name the line and the key, never the value: a topic URL is as good as a password.

use alloc::string::String;
use core::fmt;
use core::str::FromStr as _;

use smoltcp::wire::Ipv4Address;
use snafu::Snafu;

pub const URL_KEY: &str = "NOTIFY_URL";
pub const FORMAT_KEY: &str = "NOTIFY_FORMAT";
pub const EVENTS_KEY: &str = "NOTIFY_EVENTS";

const DEFAULT_PORT: u16 = 80;
const NAME_MAX: usize = 253;
const LABEL_MAX: usize = 63;

/// Something worth telling the user about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// A pass over every test finished.
    Pass,
    /// The first fault of the run was found.
    Fault,
    /// The run ended.
    End,
}

impl Event {
    pub const ALL: [Self; 3] = [Self::Pass, Self::Fault, Self::End];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fault => "fault",
            Self::End => "end",
        }
    }

    const fn bit(self) -> u8 {
        1 << self as u8
    }
}

impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The events a user asked to hear about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Events(u8);

impl Events {
    pub const ALL: Self = Self(Event::Pass.bit() | Event::Fault.bit() | Event::End.bit());

    /// Exactly `events`.
    #[must_use]
    pub fn of(events: &[Event]) -> Self {
        Self(events.iter().fold(0, |bits, event| bits | event.bit()))
    }

    #[must_use]
    pub const fn contains(self, event: Event) -> bool {
        self.0 & event.bit() != 0
    }

    /// A comma-separated list of event names; `None` when it is empty or names anything else.
    fn parse(list: &str) -> Option<Self> {
        let mut bits = 0;
        for name in list.split(',') {
            let event = Event::ALL.into_iter().find(|event| event.name().eq_ignore_ascii_case(name.trim()))?;
            bits |= event.bit();
        }
        Some(Self(bits))
    }
}

impl fmt::Display for Events {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for event in Event::ALL.into_iter().filter(|event| self.contains(*event)) {
            if !first {
                f.write_str(",")?;
            }
            first = false;
            f.write_str(event.name())?;
        }
        Ok(())
    }
}

/// How the notification's body is written.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Format {
    /// The message as plain text, the title in a `Title` header: what ntfy takes.
    #[default]
    Text,
    /// `{"event":...,"title":...,"message":...}`.
    Json,
}

impl Format {
    fn parse(value: &str) -> Option<Self> {
        match value {
            v if v.eq_ignore_ascii_case("text") => Some(Self::Text),
            v if v.eq_ignore_ascii_case("json") => Some(Self::Json),
            _ => None,
        }
    }
}

/// A DNS name: dot-separated labels of 1 to 63 letters, digits and hyphens, at most 253 bytes in all,
/// whose last label is not all digits (that would be a malformed address, not a name).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostName(String);

impl HostName {
    #[must_use]
    pub fn new(name: &str) -> Option<Self> {
        let name = name.strip_suffix('.').unwrap_or(name);
        let label_ok = |label: &str| {
            (1..=LABEL_MAX).contains(&label.len())
                && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        };
        let last = name.rsplit('.').next()?;
        let ok = name.len() <= NAME_MAX && name.split('.').all(label_ok) && !last.bytes().all(|b| b.is_ascii_digit());
        ok.then(|| Self(name.into()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Who to connect to: an address as given, or a name to look up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Host {
    Addr(Ipv4Address),
    Name(HostName),
}

impl fmt::Display for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Addr(addr) => write!(f, "{addr}"),
            Self::Name(name) => f.write_str(name.as_str()),
        }
    }
}

/// An `http://` URL taken apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub host: Host,
    pub port: u16,
    /// What follows the authority: the path and any query, starting with `/`.
    pub target: String,
}

impl Endpoint {
    /// The `Host` header's value: the port is left out when it is the default.
    #[must_use]
    pub fn host_header(&self) -> String {
        if self.port == DEFAULT_PORT {
            alloc::format!("{}", self.host)
        } else {
            alloc::format!("{}:{}", self.host, self.port)
        }
    }

    fn parse(url: &str) -> Result<Self, Problem> {
        let scheme_end = url.find("://").ok_or(Problem::NotHttp)?;
        let (scheme, rest) = (&url[..scheme_end], &url[scheme_end + 3..]);
        if scheme.eq_ignore_ascii_case("https") {
            return Err(Problem::Https);
        }
        if !scheme.eq_ignore_ascii_case("http") {
            return Err(Problem::NotHttp);
        }
        let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(authority_end);
        if authority.contains('@') {
            return Err(Problem::UserInfo);
        }
        if authority.starts_with('[') {
            return Err(Problem::Ipv6);
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (host, port.parse::<u16>().ok().filter(|port| *port != 0).ok_or(Problem::Port)?),
            None => (authority, DEFAULT_PORT),
        };
        let host = Ipv4Address::from_str(host)
            .map(Host::Addr)
            .ok()
            .or_else(|| HostName::new(host).map(Host::Name))
            .ok_or(Problem::Host)?;
        let target = match tail {
            "" => String::from("/"),
            t if t.starts_with('?') => alloc::format!("/{t}"),
            t => t.into(),
        };
        if !target.bytes().all(|b| b.is_ascii_graphic()) || target.contains('#') {
            return Err(Problem::Target);
        }
        Ok(Self { host, port, target })
    }
}

/// What is wrong with a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Problem {
    Https,
    NotHttp,
    UserInfo,
    Ipv6,
    Host,
    Port,
    Target,
    Format,
    Events,
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Https => "https is not supported (no TLS), use an http:// URL",
            Self::NotHttp => "not an http:// URL",
            Self::UserInfo => "a user name or password in the URL is not supported",
            Self::Ipv6 => "IPv6 addresses are not supported",
            Self::Host => "not an IPv4 address or a host name",
            Self::Port => "the port is not 1 to 65535",
            Self::Target => "the path has spaces, control characters or a fragment",
            Self::Format => "expected text or json",
            Self::Events => "expected a comma-separated list of pass, fault and end",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Snafu)]
pub enum ConfigError {
    #[snafu(display("not valid UTF-8"))]
    Encoding,
    #[snafu(display("line {line} is not KEY=VALUE"))]
    Line { line: usize },
    #[snafu(display("line {line} has an unknown NOTIFY_ key"))]
    UnknownKey { line: usize },
    #[snafu(display("line {line}: {key}: {problem}"))]
    Value { line: usize, key: &'static str, problem: Problem },
    #[snafu(display("no {URL_KEY} line"))]
    MissingUrl,
}

/// Where to send notifications, in what format, for which events.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub endpoint: Endpoint,
    pub format: Format,
    pub events: Events,
}

impl Config {
    /// The settings in `text`: `NOTIFY_URL` (required), `NOTIFY_FORMAT` (`text` or `json`, default
    /// text) and `NOTIFY_EVENTS` (default all). Other lines are ignored unless they start with
    /// `NOTIFY_`; a key given twice takes its last line; one outer pair of double quotes is stripped.
    pub fn from_env(text: &[u8]) -> Result<Self, ConfigError> {
        let text = core::str::from_utf8(text).map_err(|_| ConfigError::Encoding)?;
        let (mut endpoint, mut format, mut events) = (None, Format::default(), Events::ALL);
        for (index, line) in text.split('\n').enumerate() {
            let line_no = index + 1;
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line.split_once('=').ok_or(ConfigError::Line { line: line_no })?;
            let key = key.trim();
            let value = value.trim();
            let value = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(value);
            let bad = |key, problem| ConfigError::Value { line: line_no, key, problem };
            match key {
                URL_KEY => endpoint = Some(Endpoint::parse(value).map_err(|p| bad(URL_KEY, p))?),
                FORMAT_KEY => format = Format::parse(value).ok_or_else(|| bad(FORMAT_KEY, Problem::Format))?,
                EVENTS_KEY => events = Events::parse(value).ok_or_else(|| bad(EVENTS_KEY, Problem::Events))?,
                other if other.starts_with("NOTIFY_") => return Err(ConfigError::UnknownKey { line: line_no }),
                _ => {},
            }
        }
        Ok(Self { endpoint: endpoint.ok_or(ConfigError::MissingUrl)?, format, events })
    }
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};
    use rstest::rstest;

    use super::*;

    fn endpoint(host: Host, port: u16, target: &str) -> Endpoint {
        Endpoint { host, port, target: target.into() }
    }

    fn name(n: &str) -> Host {
        Host::Name(HostName::new(n).unwrap())
    }

    fn addr(a: u8, b: u8, c: u8, d: u8) -> Host {
        Host::Addr(Ipv4Address::new(a, b, c, d))
    }

    #[rstest]
    #[case::ntfy_topic("NOTIFY_URL=http://ntfy.sh/ferrite-abc", endpoint(name("ntfy.sh"), 80, "/ferrite-abc"))]
    #[case::port_and_address("NOTIFY_URL=http://192.0.2.5:8080/t", endpoint(addr(192, 0, 2, 5), 8080, "/t"))]
    #[case::no_path("NOTIFY_URL=http://lab.example", endpoint(name("lab.example"), 80, "/"))]
    #[case::query_only("NOTIFY_URL=http://lab.example:81?x=1", endpoint(name("lab.example"), 81, "/?x=1"))]
    #[case::quoted_and_loud("NOTIFY_URL=\"HTTP://Lab.Example/a/b?c=d\"", endpoint(name("Lab.Example"), 80, "/a/b?c=d"))]
    #[case::rooted_name("NOTIFY_URL=http://lab.example./t", endpoint(name("lab.example"), 80, "/t"))]
    fn urls(#[case] line: &str, #[case] want: Endpoint) {
        assert!(let Ok(config) = Config::from_env(line.as_bytes()));
        check!(config.endpoint == want);
        check!(config.format == Format::Text);
        check!(config.events == Events::ALL);
    }

    #[rstest]
    #[case::https("NOTIFY_URL=https://ntfy.sh/t", Problem::Https)]
    #[case::ftp("NOTIFY_URL=ftp://x/t", Problem::NotHttp)]
    #[case::no_scheme("NOTIFY_URL=ntfy.sh/t", Problem::NotHttp)]
    #[case::user_info("NOTIFY_URL=http://u:p@host/t", Problem::UserInfo)]
    #[case::ipv6("NOTIFY_URL=http://[::1]/t", Problem::Ipv6)]
    #[case::port_zero("NOTIFY_URL=http://host:0/t", Problem::Port)]
    #[case::port_big("NOTIFY_URL=http://host:65536/t", Problem::Port)]
    #[case::port_text("NOTIFY_URL=http://host:x/t", Problem::Port)]
    #[case::empty_host("NOTIFY_URL=http:///t", Problem::Host)]
    #[case::short_address("NOTIFY_URL=http://10.1.2/t", Problem::Host)]
    #[case::underscore("NOTIFY_URL=http://a_b.example/t", Problem::Host)]
    #[case::hyphen_edge("NOTIFY_URL=http://-a.example/t", Problem::Host)]
    #[case::space("NOTIFY_URL=http://host/a b", Problem::Target)]
    #[case::fragment("NOTIFY_URL=http://host/a#b", Problem::Target)]
    fn bad_urls_are_named_by_problem(#[case] line: &str, #[case] problem: Problem) {
        check!(Config::from_env(line.as_bytes()) == Err(ConfigError::Value { line: 1, key: URL_KEY, problem }));
    }

    #[rstest]
    #[case::format_json("NOTIFY_URL=http://h/t\nNOTIFY_FORMAT=JSON", Format::Json, Events::ALL)]
    #[case::format_text("NOTIFY_URL=http://h/t\nNOTIFY_FORMAT=\"text\"", Format::Text, Events::ALL)]
    #[case::events(
        "NOTIFY_URL=http://h/t\nNOTIFY_EVENTS=pass, END",
        Format::Text,
        Events::of(&[Event::Pass, Event::End])
    )]
    #[case::last_wins(
        "NOTIFY_FORMAT=json\r\nNOTIFY_URL=http://h/t\r\nNOTIFY_FORMAT=text\r\nNOTIFY_EVENTS=fault\r\n",
        Format::Text,
        Events::of(&[Event::Fault])
    )]
    #[case::other_lines_ignored("# note\n\nWPA_SSID=x\nNOTIFY_URL=http://h/t\n", Format::Text, Events::ALL)]
    fn options(#[case] text: &str, #[case] format: Format, #[case] events: Events) {
        assert!(let Ok(config) = Config::from_env(text.as_bytes()));
        check!(config.format == format);
        check!(config.events == events);
    }

    #[rstest]
    #[case::empty("", ConfigError::MissingUrl)]
    #[case::comments_only("# NOTIFY_URL=http://h/t\n", ConfigError::MissingUrl)]
    #[case::not_a_pair("NOTIFY_URL http://h/t", ConfigError::Line { line: 1 })]
    #[case::second_line("\nNOTIFY_URL=http://h/t\nnonsense", ConfigError::Line { line: 3 })]
    #[case::unknown_key("NOTIFY_URL=http://h/t\nNOTIFY_TOPIC=x", ConfigError::UnknownKey { line: 2 })]
    #[case::format(
        "NOTIFY_URL=http://h/t\nNOTIFY_FORMAT=xml",
        ConfigError::Value { line: 2, key: FORMAT_KEY, problem: Problem::Format }
    )]
    #[case::events_unknown(
        "NOTIFY_URL=http://h/t\nNOTIFY_EVENTS=pass,done",
        ConfigError::Value { line: 2, key: EVENTS_KEY, problem: Problem::Events }
    )]
    #[case::events_empty(
        "NOTIFY_URL=http://h/t\nNOTIFY_EVENTS=",
        ConfigError::Value { line: 2, key: EVENTS_KEY, problem: Problem::Events }
    )]
    fn broken_files(#[case] text: &str, #[case] error: ConfigError) {
        check!(Config::from_env(text.as_bytes()) == Err(error));
    }

    #[test]
    fn a_file_that_is_not_text_is_refused() {
        check!(Config::from_env(b"NOTIFY_URL=http://h/\xff") == Err(ConfigError::Encoding));
    }

    #[test]
    fn errors_never_print_the_value() {
        let error = Config::from_env(b"NOTIFY_URL=https://ntfy.sh/hunter2-secret").unwrap_err();
        let shown = alloc::format!("{error}");
        check!(shown.contains("line 1") && shown.contains(URL_KEY));
        check!(!shown.contains("hunter2"));
    }

    #[rstest]
    #[case::explicit_port("NOTIFY_URL=http://ntfy.local:8080/t", "ntfy.local:8080")]
    #[case::default_port("NOTIFY_URL=http://ntfy.local/t", "ntfy.local")]
    fn the_host_header_omits_the_default_port(#[case] line: &str, #[case] header: &str) {
        let config = Config::from_env(line.as_bytes()).unwrap();
        check!(config.endpoint.host_header() == header);
    }
}
