//! The notification settings: where to POST, in what body format, for which events.
//!
//! A [`Problem`] never carries the URL: a topic URL is as good as a password.

use alloc::string::String;
use core::fmt;
use core::str::FromStr as _;

use ferrite_hal::Set;
use smoltcp::wire::Ipv4Address;

const DEFAULT_PORT: u16 = 80;
const NAME_MAX: usize = 253;
const LABEL_MAX: usize = 63;

/// Something worth telling the user about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::IntoStaticStr, strum::VariantArray, strum::Display)]
#[strum(serialize_all = "lowercase")]
pub enum Event {
    /// A pass over every test finished.
    Pass,
    /// The first fault of the run was found.
    Fault,
    /// The run ended.
    End,
}

ferrite_hal::flag!(Event);

/// The events a user asked to hear about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Events(Set<Event>);

impl Events {
    pub const ALL: Self = Self(Set::ALL);

    /// Exactly `events`.
    #[must_use]
    pub fn of(events: &[Event]) -> Self {
        Self(events.iter().copied().collect())
    }

    #[must_use]
    pub fn contains(self, event: Event) -> bool {
        self.0.has(event)
    }
}

/// The names separated by commas.
impl fmt::Display for Events {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut names = self.0.iter().map(<&str>::from);
        if let Some(first) = names.next() {
            f.write_str(first)?;
            names.try_for_each(|name| write!(f, ",{name}"))?;
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

impl Host {
    /// An IPv4 address, else a host name.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Ipv4Address::from_str(text).map(Self::Addr).ok().or_else(|| HostName::new(text).map(Self::Name))
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

    /// An `http://` URL taken apart. Only plain HTTP to an IPv4 address or a host name, with no user
    /// information.
    pub fn parse(url: &str) -> Result<Self, Problem> {
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
        let host = Host::parse(host).ok_or(Problem::Host)?;
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
        })
    }
}

/// Where to send notifications, in what format, for which events.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub endpoint: Endpoint,
    pub format: Format,
    pub events: Events,
}

impl Config {
    /// The settings for the URL `url`.
    pub fn new(url: &str, format: Format, events: Events) -> Result<Self, Problem> {
        Ok(Self { endpoint: Endpoint::parse(url)?, format, events })
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
    #[case::ntfy_topic("http://ntfy.sh/ferrite-abc", endpoint(name("ntfy.sh"), 80, "/ferrite-abc"))]
    #[case::port_and_address("http://192.0.2.5:8080/t", endpoint(addr(192, 0, 2, 5), 8080, "/t"))]
    #[case::no_path("http://lab.example", endpoint(name("lab.example"), 80, "/"))]
    #[case::query_only("http://lab.example:81?x=1", endpoint(name("lab.example"), 81, "/?x=1"))]
    #[case::loud("HTTP://Lab.Example/a/b?c=d", endpoint(name("Lab.Example"), 80, "/a/b?c=d"))]
    #[case::rooted_name("http://lab.example./t", endpoint(name("lab.example"), 80, "/t"))]
    fn urls(#[case] url: &str, #[case] want: Endpoint) {
        assert!(let Ok(config) = Config::new(url, Format::Text, Events::ALL));
        check!(config.endpoint == want);
        check!(config.format == Format::Text);
        check!(config.events == Events::ALL);
    }

    #[rstest]
    #[case::https("https://ntfy.sh/t", Problem::Https)]
    #[case::ftp("ftp://x/t", Problem::NotHttp)]
    #[case::no_scheme("ntfy.sh/t", Problem::NotHttp)]
    #[case::user_info("http://u:p@host/t", Problem::UserInfo)]
    #[case::ipv6("http://[::1]/t", Problem::Ipv6)]
    #[case::port_zero("http://host:0/t", Problem::Port)]
    #[case::port_big("http://host:65536/t", Problem::Port)]
    #[case::port_text("http://host:x/t", Problem::Port)]
    #[case::empty_host("http:///t", Problem::Host)]
    #[case::short_address("http://10.1.2/t", Problem::Host)]
    #[case::underscore("http://a_b.example/t", Problem::Host)]
    #[case::hyphen_edge("http://-a.example/t", Problem::Host)]
    #[case::space("http://host/a b", Problem::Target)]
    #[case::fragment("http://host/a#b", Problem::Target)]
    fn bad_urls_are_named_by_problem(#[case] url: &str, #[case] problem: Problem) {
        check!(Config::new(url, Format::Text, Events::ALL) == Err(problem));
    }

    #[test]
    fn a_problem_never_prints_the_url() {
        let problem = Config::new("https://ntfy.sh/hunter2-secret", Format::Text, Events::ALL).unwrap_err();
        check!(!alloc::format!("{problem} {problem:?}").contains("hunter2"));
    }

    #[rstest]
    #[case::explicit_port("http://ntfy.local:8080/t", "ntfy.local:8080")]
    #[case::default_port("http://ntfy.local/t", "ntfy.local")]
    fn the_host_header_omits_the_default_port(#[case] url: &str, #[case] header: &str) {
        let config = Config::new(url, Format::Text, Events::ALL).unwrap();
        check!(config.endpoint.host_header() == header);
    }
}
