//! The byte link to the target: which transport carries it, and a boxed `Read + Write` over it.

use std::fmt;
use std::io::{self, Read, Write};

use clap::ValueEnum;

pub const DEFAULT_IN_TRANSFERS: usize = 16;
pub const DEFAULT_IN_SIZE: usize = 16 * 1024;
pub const DEFAULT_OUT_TRANSFERS: usize = 8;
pub const DEFAULT_OUT_SIZE: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum, strum::IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
pub enum Transport {
    /// The device through usbfs, with many large transfers in flight
    #[default]
    Usb,
    /// The `usb_debug` tty
    Tty,
}

impl fmt::Display for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(<&str>::from(self))
    }
}

/// How many transfers the USB transport keeps in flight, and how large each one is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsbConfig {
    pub in_transfers: usize,
    pub in_size: usize,
    pub out_transfers: usize,
    pub out_size: usize,
}

impl Default for UsbConfig {
    fn default() -> Self {
        Self {
            in_transfers: DEFAULT_IN_TRANSFERS,
            in_size: DEFAULT_IN_SIZE,
            out_transfers: DEFAULT_OUT_TRANSFERS,
            out_size: DEFAULT_OUT_SIZE,
        }
    }
}

impl fmt::Display for UsbConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} x {} in, {} x {} out",
            self.in_transfers,
            Bytes(self.in_size),
            self.out_transfers,
            Bytes(self.out_size)
        )
    }
}

struct Bytes(usize);

impl fmt::Display for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 != 0 && self.0.is_multiple_of(1024) {
            write!(f, "{} KiB", self.0 / 1024)
        } else {
            write!(f, "{} B", self.0)
        }
    }
}

/// What a command needs to know to open its link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LinkOptions {
    pub transport: Transport,
    pub usb: UsbConfig,
}

/// Anything the benchmark and the capture can talk over.
pub trait Channel: Read + Write {}

impl<T: Read + Write> Channel for T {}

/// An open link: the transport it runs over, a short description of it, and the byte stream.
pub struct Link {
    transport: Transport,
    detail: String,
    channel: Box<dyn Channel>,
}

impl Link {
    pub fn new(transport: Transport, detail: impl Into<String>, channel: impl Channel + 'static) -> Self {
        Self { transport, detail: detail.into(), channel: Box::new(channel) }
    }

    pub const fn transport(&self) -> Transport {
        self.transport
    }

    /// The transport and its parameters, like `usb (16 x 16 KiB in, 8 x 64 KiB out)`.
    pub fn describe(&self) -> String {
        if self.detail.is_empty() {
            self.transport.to_string()
        } else {
            format!("{} ({})", self.transport, self.detail)
        }
    }
}

impl Read for Link {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.channel.read(buf)
    }
}

impl Write for Link {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.channel.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.channel.flush()
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;
    use crate::testing::FakeLink;

    #[test]
    fn a_link_describes_its_transport_and_passes_bytes_through() {
        let mut link = Link::new(Transport::Usb, "x", FakeLink::new().with_rx(b"hi"));
        check!(link.describe() == "usb (x)");
        let mut buf = [0; 4];
        check!(link.read(&mut buf).ok() == Some(2));
        check!(Link::new(Transport::Tty, "", FakeLink::new()).describe() == "tty");
    }
}
