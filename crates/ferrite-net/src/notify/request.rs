//! The bytes of a notification's POST, and the status line of the reply.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::{self, Write as _};

use super::config::{Endpoint, Event, Format};

/// Longest status line read before the reply is called malformed.
const STATUS_LINE_MAX: usize = 96;

/// One thing to tell the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    pub event: Event,
    pub title: String,
    pub message: String,
}

impl Notification {
    pub fn new(event: Event, title: impl Into<String>, message: impl Into<String>) -> Self {
        Self { event, title: title.into(), message: message.into() }
    }
}

/// `text` with every character a header cannot carry (anything but printable ASCII) dropped.
fn header_safe(text: &str) -> String {
    text.chars().filter(|c| c.is_ascii() && !c.is_ascii_control()).collect::<String>().trim().into()
}

fn json_string(out: &mut String, text: &str) -> fmt::Result {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => write!(out, "\\u{:04x}", u32::from(c))?,
            c => out.push(c),
        }
    }
    out.push('"');
    Ok(())
}

/// The body of `notification` in `format`.
fn body(format: Format, notification: &Notification) -> String {
    match format {
        Format::Text => notification.message.clone(),
        Format::Json => {
            let mut out = String::from("{\"event\":");
            // Writing into a String cannot fail.
            let _ = json_string(&mut out, notification.event.name());
            out.push_str(",\"title\":");
            let _ = json_string(&mut out, &notification.title);
            out.push_str(",\"message\":");
            let _ = json_string(&mut out, &notification.message);
            out.push('}');
            out
        },
    }
}

/// The whole HTTP/1.1 request for `notification`, closing the connection after the reply. Plain
/// text carries the title in a `Title` header, as ntfy reads it; JSON carries it in the body.
#[must_use]
pub fn encode(endpoint: &Endpoint, format: Format, notification: &Notification) -> Vec<u8> {
    let body = body(format, notification);
    let content_type = match format {
        Format::Text => "text/plain; charset=utf-8",
        Format::Json => "application/json",
    };
    let mut head = String::with_capacity(160);
    // Writing into a String cannot fail.
    let _ = write!(
        head,
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n",
        endpoint.target,
        endpoint.host_header(),
        body.len()
    );
    let title = header_safe(&notification.title);
    if format == Format::Text && !title.is_empty() {
        let _ = write!(head, "Title: {title}\r\n");
    }
    head.push_str("\r\n");
    head.push_str(&body);
    head.into_bytes()
}

/// What the bytes read so far of a reply say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply {
    /// The status line is not complete yet.
    Incomplete,
    /// Not an HTTP/1.x status line.
    Malformed,
    Status(u16),
}

/// The status code in the first line of `bytes`, an HTTP/1.x reply read so far.
#[must_use]
pub fn parse_status_line(bytes: &[u8]) -> Reply {
    let Some(end) = bytes.iter().position(|&b| b == b'\n') else {
        return if bytes.len() > STATUS_LINE_MAX { Reply::Malformed } else { Reply::Incomplete };
    };
    let line = bytes[..end].strip_suffix(b"\r").unwrap_or(&bytes[..end]);
    let Some(rest) = line.strip_prefix(b"HTTP/1.").and_then(|r| r.split_first()) else {
        return Reply::Malformed;
    };
    let (&minor, rest) = rest;
    let Some(rest) = rest.strip_prefix(b" ").filter(|_| minor.is_ascii_digit()) else {
        return Reply::Malformed;
    };
    match rest {
        [a, b, c, tail @ ..]
            if [a, b, c].iter().all(|d| d.is_ascii_digit()) && matches!(tail.first(), None | Some(b' ')) =>
        {
            Reply::Status(u16::from(a - b'0') * 100 + u16::from(b - b'0') * 10 + u16::from(c - b'0'))
        },
        _ => Reply::Malformed,
    }
}

/// What a status code means for a notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Delivered,
    /// Worth trying again: the server is busy or broken, not unwilling.
    Retry,
    /// The server refused it; the same request will be refused again.
    Rejected,
}

#[must_use]
pub const fn verdict(status: u16) -> Verdict {
    match status {
        200..=299 => Verdict::Delivered,
        408 | 425 | 429 | 500..=599 => Verdict::Retry,
        _ => Verdict::Rejected,
    }
}

#[cfg(test)]
mod tests {
    use alloc::format;

    use assert2::check;
    use rstest::rstest;

    use super::*;
    use crate::notify::{Config, Events};

    fn endpoint(url: &str) -> Endpoint {
        Config::new(url, Format::Text, Events::ALL).unwrap().endpoint
    }

    fn pass() -> Notification {
        Notification::new(
            Event::Pass,
            "ferrite pass 3",
            "ferrite: pass 3 done on ferrite.local, 0 errors, 15.9 GiB, 41:12",
        )
    }

    #[test]
    fn text_is_a_plain_body_with_the_title_in_a_header() {
        let bytes = encode(&endpoint("http://ntfy.local:8080/ferrite-topic"), Format::Text, &pass());
        let message = "ferrite: pass 3 done on ferrite.local, 0 errors, 15.9 GiB, 41:12";
        check!(
            bytes
                == format!(
                    "POST /ferrite-topic HTTP/1.1\r\nHost: ntfy.local:8080\r\nContent-Type: text/plain; charset=utf-8\r\n\
                     Content-Length: {}\r\nConnection: close\r\nTitle: ferrite pass 3\r\n\r\n{message}",
                    message.len()
                )
                .into_bytes()
        );
    }

    #[test]
    fn json_carries_the_title_in_the_body_and_escapes() {
        let note = Notification::new(Event::Fault, "a \"b\"", "line\nbreak \\ tab\t bell\u{7} caf\u{e9}");
        let bytes = encode(&endpoint("http://10.0.0.2/hook"), Format::Json, &note);
        let body = r#"{"event":"fault","title":"a \"b\"","message":"line\nbreak \\ tab\t bell\u0007 café"}"#;
        check!(
            bytes
                == format!(
                    "POST /hook HTTP/1.1\r\nHost: 10.0.0.2\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                )
                .into_bytes()
        );
    }

    #[test]
    fn a_title_cannot_inject_headers() {
        let note = Notification::new(Event::End, "ok\r\nX-Evil: 1 \u{e9}", "m");
        let text = String::from_utf8(encode(&endpoint("http://h/t"), Format::Text, &note)).unwrap();
        check!(text.contains("\r\nTitle: okX-Evil: 1\r\n"));
        check!(!text.contains("\r\nX-Evil"));
    }

    #[rstest]
    #[case::ok(b"HTTP/1.1 200 OK\r\nServer: x\r\n\r\n", Reply::Status(200))]
    #[case::no_reason(b"HTTP/1.1 202 \r\n", Reply::Status(202))]
    #[case::bare_code(b"HTTP/1.0 404\r\n", Reply::Status(404))]
    #[case::bare_newline(b"HTTP/1.1 503 Busy\n", Reply::Status(503))]
    #[case::partial(b"HTTP/1.1 20", Reply::Incomplete)]
    #[case::empty(b"", Reply::Incomplete)]
    #[case::not_http(b"SSH-2.0-OpenSSH\r\n", Reply::Malformed)]
    #[case::http2(b"HTTP/2 200\r\n", Reply::Malformed)]
    #[case::letters(b"HTTP/1.1 2x0 OK\r\n", Reply::Malformed)]
    #[case::four_digits(b"HTTP/1.1 2000 OK\r\n", Reply::Malformed)]
    #[case::no_space(b"HTTP/1.1200 OK\r\n", Reply::Malformed)]
    #[case::endless_first_line(&[b'x'; 200], Reply::Malformed)]
    fn status_lines(#[case] bytes: &[u8], #[case] want: Reply) {
        check!(parse_status_line(bytes) == want);
    }

    #[rstest]
    #[case(200, Verdict::Delivered)]
    #[case(204, Verdict::Delivered)]
    #[case(301, Verdict::Rejected)]
    #[case(400, Verdict::Rejected)]
    #[case(404, Verdict::Rejected)]
    #[case(408, Verdict::Retry)]
    #[case(429, Verdict::Retry)]
    #[case(503, Verdict::Retry)]
    fn verdicts(#[case] status: u16, #[case] want: Verdict) {
        check!(verdict(status) == want);
    }
}
