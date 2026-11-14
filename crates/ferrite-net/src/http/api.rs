//! What a request may ask of the server before the site sees it: the headers every request must
//! pass and the replies for those that do not.
//!
//! Control requests must say `Content-Type: application/json`. A plain HTML form can only send
//! form or text bodies, and a cross-origin script that sets this type has to pass a CORS preflight,
//! which the server never grants, so another site cannot drive the controls from a visitor's browser.
//!
//! Every request must also be addressed to this machine: a page on another site whose name has been
//! re-pointed at one of our addresses (DNS rebinding) would otherwise be same-origin with the API and
//! read it freely. Its requests carry its own name in `Host`, which is refused.

use core::fmt::{self, Write as _};
use core::net::Ipv6Addr;

use smoltcp::wire::Ipv4Address;
use snafu::Snafu;

use crate::mdns::HOST;

/// Control bodies longer than this are refused unread.
pub const BODY_CAP: usize = 256;

/// Room for a short reply (an error or an acknowledgement) on the stack.
pub const REPLY_CAP: usize = 256;

/// A short reply, built without touching the heap.
pub type Reply = heapless::String<REPLY_CAP>;

/// A body parse error as far as it is worth echoing back.
pub type Reason = heapless::String<80>;

/// What a request resolved to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route<C> {
    /// The static page.
    Page,
    /// The cached document, as a response.
    Document,
    /// The cached document as server-sent events.
    Events,
    /// A control request, with its body to be read and parsed.
    Control(C),
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
    #[snafu(display("bad body: {reason}"))]
    BadBody { reason: Reason },
    #[snafu(display("too many event streams open, poll the document instead"))]
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

    /// A body parse error as a rejection.
    pub fn bad_body(error: &impl fmt::Display) -> Self {
        let mut reason = Reason::new();
        // A message longer than the buffer is cut, which is as much as is worth sending.
        let _ = write!(reason, "{error}");
        Self::BadBody { reason }
    }
}

/// The cross-site guard: the media type must be `application/json`, parameters allowed.
pub fn check_json(content_type: Option<&[u8]>) -> Result<(), Rejection> {
    let media = content_type.and_then(|value| value.split(|&b| b == b';').next()).map(<[u8]>::trim_ascii);
    if media.is_some_and(|m| m.eq_ignore_ascii_case(b"application/json")) { Ok(()) } else { Err(Rejection::NotJson) }
}

/// Whether `host`, the value of a `Host` header, names this machine: any of its `addresses`, its mDNS
/// name or loopback (what a forwarded port arrives as), with or without a port.
pub fn check_host(host: Option<&[u8]>, addresses: &[Ipv4Address]) -> Result<(), Rejection> {
    let host = host.ok_or(Rejection::NoHost)?;
    if split_port(host).is_some_and(|name| names_us(name, addresses)) { Ok(()) } else { Err(Rejection::Misdirected) }
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

fn names_us(name: &[u8], addresses: &[Ipv4Address]) -> bool {
    if let Some(inner) = name.strip_prefix(b"[").and_then(|n| n.strip_suffix(b"]")) {
        return core::str::from_utf8(inner)
            .ok()
            .and_then(|s| s.parse::<Ipv6Addr>().ok())
            .is_some_and(|ip| ip.is_loopback());
    }
    if let Some(ip) = core::str::from_utf8(name).ok().and_then(|s| s.parse::<Ipv4Address>().ok()) {
        return ip.is_loopback() || addresses.contains(&ip);
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

/// Writes what is written through it as the inside of a JSON string.
pub struct JsonEscape<'a>(pub &'a mut dyn fmt::Write);

impl fmt::Write for JsonEscape<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let needs_escape = |c: char| matches!(c, '"' | '\\') || u32::from(c) < 0x20;
        let mut rest = s;
        while let Some(at) = rest.find(needs_escape) {
            let (plain, special) = rest.split_at(at);
            self.0.write_str(plain)?;
            let mut chars = special.chars();
            match chars.next() {
                Some('"') => self.0.write_str("\\\"")?,
                Some('\\') => self.0.write_str("\\\\")?,
                Some('\n') => self.0.write_str("\\n")?,
                Some(c) => write!(self.0, "\\u{:04x}", u32::from(c))?,
                None => {},
            }
            rest = chars.as_str();
        }
        self.0.write_str(rest)
    }
}

/// `{"error": message}`.
pub fn error(message: impl fmt::Display) -> Reply {
    let mut out = Reply::new();
    // The longest rejection fits the buffer; a longer message from elsewhere is cut.
    let _ = out.write_str("{\"error\":\"");
    let _ = write!(JsonEscape(&mut out), "{message}");
    let _ = out.write_str("\"}");
    out
}

/// `{"accepted": name}`, the reply to a control request.
pub fn accepted(name: &str) -> Reply {
    let mut out = Reply::new();
    let _ = out.write_str("{\"accepted\":\"");
    let _ = write!(JsonEscape(&mut out), "{name}");
    let _ = out.write_str("\"}");
    out
}

#[cfg(test)]
mod tests {
    use alloc::format;
    use alloc::string::String;

    use assert2::check;
    use rstest::rstest;

    use super::*;

    const LEASE: Ipv4Address = Ipv4Address::new(10, 0, 2, 15);
    const OTHER_PORT: Ipv4Address = Ipv4Address::new(192, 0, 2, 40);
    const BOTH: [Ipv4Address; 2] = [LEASE, OTHER_PORT];

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
    #[case::address("10.0.2.15")]
    #[case::address_port("10.0.2.15:80")]
    #[case::second_port_address("192.0.2.40")]
    #[case::second_port_address_port("192.0.2.40:8080")]
    #[case::mdns_name("ferrite.local")]
    #[case::mdns_name_shouting("FERRITE.Local:8080")]
    #[case::mdns_name_rooted("ferrite.local.")]
    #[case::loopback("127.0.0.1:8081")]
    #[case::loopback_range("127.5.5.5")]
    #[case::localhost("localhost:8081")]
    #[case::loopback_v6("[::1]:8081")]
    #[case::loopback_v6_bare("[::1]")]
    fn hosts_that_are_ours(#[case] host: &str) {
        check!(check_host(Some(host.as_bytes()), &BOTH) == Ok(()));
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
        check!(check_host(Some(host.as_bytes()), &BOTH) == Err(Rejection::Misdirected));
    }

    #[test]
    fn without_a_lease_only_the_name_and_loopback_pass() {
        check!(check_host(Some(b"10.0.2.15"), &[]) == Err(Rejection::Misdirected));
        check!(check_host(Some(b"ferrite.local"), &[]) == Ok(()));
        check!(check_host(Some(b"127.0.0.1"), &[]) == Ok(()));
    }

    #[test]
    fn a_missing_host_is_a_bad_request() {
        check!(check_host(None, &BOTH) == Err(Rejection::NoHost));
        check!(Rejection::NoHost.status() == 400);
        check!(Rejection::Misdirected.status() == 421);
    }

    #[test]
    fn replies_are_escaped_and_fit_their_buffer() {
        check!(error("a\"b\\c\nd") == "{\"error\":\"a\\\"b\\\\c\\nd\"}");
        check!(accepted("pause") == "{\"accepted\":\"pause\"}");
        let long = Rejection::bad_body(&"\u{7f}".repeat(300));
        let reply = error(&long);
        check!(reply.len() <= REPLY_CAP);
        check!(reply.ends_with("\"}"), "{reply}");
        let text: String = format!("{long}");
        check!(text.starts_with("bad body: "));
    }
}
