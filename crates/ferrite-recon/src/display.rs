//! `Display` wrappers for report lines.

use core::fmt;
use core::time::Duration;

use ferrite_hal::Instant;
use ferrite_xhci::PortRange;

/// A duration in milliseconds with microsecond digits, like `12.345ms`.
#[derive(Clone, Copy, Debug)]
pub struct Ms(pub Duration);

impl fmt::Display for Ms {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let micros = self.0.as_micros();
        write!(f, "{}.{:03}ms", micros / 1000, micros % 1000)
    }
}

/// A milestone as time since `start`, or `-` if it was never reached.
#[derive(Clone, Copy, Debug)]
pub struct Since {
    pub start: Instant,
    pub at: Option<Instant>,
}

impl fmt::Display for Since {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.at {
            Some(at) => write!(f, "{}", Ms(at - self.start)),
            None => f.write_str("-"),
        }
    }
}

/// Throughput in decimal megabytes per second, like `3.398MB/s`.
#[derive(Clone, Copy, Debug)]
pub struct Rate {
    pub bytes: u64,
    pub over: Duration,
}

impl fmt::Display for Rate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let micros = self.over.as_micros();
        if micros == 0 {
            return f.write_str("-");
        }
        let milli = u128::from(self.bytes) * 1000 / micros;
        write!(f, "{}.{:03}MB/s", milli / 1000, milli % 1000)
    }
}

/// An error followed by each error it wraps, joined with `: `.
pub struct Chain<'a>(pub &'a dyn core::error::Error);

impl fmt::Display for Chain<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)?;
        let mut source = self.0.source();
        while let Some(err) = source {
            write!(f, ": {err}")?;
            source = err.source();
        }
        Ok(())
    }
}

/// USB 3 port ranges as `1-2,5-6`, or `none`.
pub struct Ranges<'a>(pub &'a [PortRange]);

impl fmt::Display for Ranges<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return f.write_str("none");
        }
        for (i, range) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(",")?;
            }
            write!(f, "{range}")?;
        }
        Ok(())
    }
}

/// An optional value in hex, or `none`.
pub struct HexOrNone<T>(pub Option<T>);

impl<T: fmt::LowerHex> fmt::Display for HexOrNone<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Some(v) => write!(f, "{v:#x}"),
            None => f.write_str("none"),
        }
    }
}

#[must_use]
pub const fn yes_no(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}
