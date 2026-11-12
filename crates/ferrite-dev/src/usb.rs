//! The DbC device driven directly through usbfs, with many large transfers in flight.
//!
//! The kernel's `usb_debug` driver is detached for the session and bound again afterwards, so
//! `/dev/ferrite-dbc` comes back even when the session ends on an error.

use std::io::{self, Read, Write};
use std::thread;
use std::time::{Duration, Instant};

use nusb::descriptors::{ConfigurationDescriptor, TransferType};
use nusb::io::{EndpointRead, EndpointWrite};
use nusb::transfer::{Bulk, Direction, In, Out};
use nusb::{Device, ErrorKind, Interface, MaybeFuture};
use snafu::{OptionExt, ResultExt, Snafu};

use crate::claim::{Claim, ClaimError, ClaimGuard};
use crate::link::{Link, Transport, UsbConfig};
use crate::tty::IDLE_SLICE;

pub const VENDOR_ID: u16 = 0x1d6b;
pub const PRODUCT_ID: u16 = 0x0010;
const INTERFACE: u8 = 0;

const WRITE_STALL: Duration = Duration::from_secs(1);
const QUIESCE_LIMIT: Duration = Duration::from_secs(2);
const REATTACH_TRIES: u32 = 20;
const REATTACH_PAUSE: Duration = Duration::from_millis(50);

pub const UDEV_RULE: &str = r#"SUBSYSTEM=="usb", ATTR{idVendor}=="1d6b", ATTR{idProduct}=="0010", TAG+="uaccess""#;

const RECOVERY: &str = "If /dev/ferrite-dbc is missing, unplug and replug the cable, or run `ferrite-dev log`, which \
                        re-enables the USB port while the device is absent; either makes the kernel bind usb_debug again.";

#[derive(Debug, Snafu)]
pub enum UsbError {
    #[snafu(display("no USB device {VENDOR_ID:04x}:{PRODUCT_ID:04x} is present"))]
    Absent,

    #[snafu(display("no permission to open {node}"))]
    Denied { node: String },

    #[snafu(display("cannot list the USB devices"))]
    List { source: nusb::Error },

    #[snafu(display("cannot open {node}"))]
    Open { node: String, source: nusb::Error },

    #[snafu(display("the device has no usable active configuration"))]
    Configuration { source: nusb::ActiveConfigurationError },

    #[snafu(display("interface {interface} has no alternate setting 0"))]
    NoInterface { interface: u8 },

    #[snafu(display("interface {interface} has no bulk {direction} endpoint"))]
    NoEndpoint { interface: u8, direction: &'static str },

    #[snafu(display("cannot detach the kernel driver from interface {interface}"))]
    Detach { interface: u8, source: nusb::Error },

    #[snafu(display("cannot claim interface {interface}"))]
    ClaimInterface { interface: u8, source: nusb::Error },

    #[snafu(display("cannot open endpoint {address:#04x}"))]
    OpenEndpoint { address: u8, source: nusb::Error },
}

/// The interface and its two bulk endpoints, read from the descriptors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Endpoints {
    pub interface: u8,
    pub bulk_in: u8,
    pub bulk_out: u8,
}

/// Find the bulk IN and bulk OUT endpoints of `interface`'s default alternate setting.
pub fn discover(config: &ConfigurationDescriptor<'_>, interface: u8) -> Result<Endpoints, UsbError> {
    let alt = config
        .interface_alt_settings()
        .find(|alt| alt.interface_number() == interface && alt.alternate_setting() == 0)
        .context(NoInterfaceSnafu { interface })?;
    let bulk = |direction| {
        alt.endpoints()
            .find(|ep| ep.transfer_type() == TransferType::Bulk && ep.direction() == direction)
            .map(|ep| ep.address())
    };
    Ok(Endpoints {
        interface,
        bulk_in: bulk(Direction::In).context(NoEndpointSnafu { interface, direction: "IN" })?,
        bulk_out: bulk(Direction::Out).context(NoEndpointSnafu { interface, direction: "OUT" })?,
    })
}

/// What to tell the user when the device node cannot be opened.
pub fn permission_help(node: &str) -> Vec<String> {
    vec![
        format!("--- cannot open {node}: permission denied ---"),
        "--- to use the usb transport, add this rule to a file in /etc/udev/rules.d/ that sorts before 73- \
         (for example 70-ferrite-dbc.rules), run `udevadm control --reload`, then replug the cable: ---"
            .to_owned(),
        UDEV_RULE.to_owned(),
        "--- falling back to the tty transport ---".to_owned(),
    ]
}

/// Opens the DbC device as a [`Link`], or says why it cannot yet.
pub trait UsbOpener {
    fn open(&mut self) -> Result<Link, UsbError>;
}

/// The reading half of a [`Duplex`].
pub trait Inbound: Read {
    /// Cancel what is in flight and wait for it to finish, for a bounded time.
    fn quiesce(&mut self);
}

/// The writing half of a [`Duplex`].
pub trait Outbound: Write {
    /// Hand the burst written so far to the host controller without waiting for it, ending it with a
    /// short or zero-length packet so the device completes its transfer. Full transfers in the
    /// middle of a stream are submitted by `write` itself, without that terminator.
    fn submit_end(&mut self);
    /// Send what is buffered and wait for it, for a bounded time.
    fn quiesce(&mut self);
}

/// Both endpoints as one `Read + Write` link.
///
/// Writes are buffered into large transfers, and a burst that is still open goes out, terminated,
/// before the next read waits or on flush, which is what a request and its reply need. A read that returns no bytes is
/// reported as idle, never as a closed link.
pub struct Duplex<R: Inbound, W: Outbound> {
    reader: R,
    writer: W,
    /// Bytes were written since the last burst was ended.
    open_burst: bool,
}

impl<R: Inbound, W: Outbound> Duplex<R, W> {
    pub const fn new(reader: R, writer: W) -> Self {
        Self { reader, writer, open_burst: false }
    }

    fn end_burst(&mut self) {
        if std::mem::take(&mut self.open_burst) {
            self.writer.submit_end();
        }
    }
}

impl<R: Inbound, W: Outbound> Read for Duplex<R, W> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.end_burst();
        match self.reader.read(buf) {
            // The device ends a burst with a zero-length packet: a terminator, not the end of the link.
            Ok(0) if !buf.is_empty() => Err(io::ErrorKind::TimedOut.into()),
            other => other.map_err(link_error),
        }
    }
}

impl<R: Inbound, W: Outbound> Write for Duplex<R, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.writer.write(buf).map_err(link_error)?;
        self.open_burst |= written > 0;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.end_burst();
        Ok(())
    }
}

impl<R: Inbound, W: Outbound> Drop for Duplex<R, W> {
    fn drop(&mut self) {
        self.writer.quiesce();
        self.reader.quiesce();
    }
}

/// A vanished device is the link contract's "far side gone", which is a broken pipe.
fn link_error(error: io::Error) -> io::Error {
    match error.kind() {
        io::ErrorKind::ConnectionAborted | io::ErrorKind::NotConnected => {
            io::Error::new(io::ErrorKind::BrokenPipe, error)
        },
        _ => error,
    }
}

impl Inbound for EndpointRead<Bulk> {
    fn quiesce(&mut self) {
        self.cancel_all();
        let deadline = Instant::now() + QUIESCE_LIMIT;
        let mut scratch = [0; 4096];
        while Instant::now() < deadline {
            match self.read(&mut scratch) {
                Ok(0) => return,
                Ok(_) => {},
                Err(e) if e.kind() == io::ErrorKind::TimedOut => {},
                Err(_) => return,
            }
        }
    }
}

impl Outbound for EndpointWrite<Bulk> {
    fn submit_end(&mut self) {
        Self::submit_end(self);
    }

    fn quiesce(&mut self) {
        // A failed or timed out flush means the device is gone or stuck; dropping the endpoint cancels the rest.
        let _ = Write::flush(self);
    }
}

/// An open session: the endpoints first, so they are gone before the interface is handed back.
pub struct UsbLink<R: Inbound, W: Outbound, C: Claim> {
    io: Duplex<R, W>,
    _claim: ClaimGuard<C>,
}

impl<R: Inbound, W: Outbound, C: Claim> UsbLink<R, W, C> {
    pub const fn new(io: Duplex<R, W>, claim: ClaimGuard<C>) -> Self {
        Self { io, _claim: claim }
    }
}

impl<R: Inbound, W: Outbound, C: Claim> Read for UsbLink<R, W, C> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.io.read(buf)
    }
}

impl<R: Inbound, W: Outbound, C: Claim> Write for UsbLink<R, W, C> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.io.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.io.flush()
    }
}

/// The interface of one device, claimed from the kernel driver and given back afterwards.
pub struct NusbClaim {
    device: Device,
    number: u8,
    interface: Option<Interface>,
}

impl NusbClaim {
    const fn new(device: Device, number: u8) -> Self {
        Self { device, number, interface: None }
    }

    fn detach(&self) -> Result<(), nusb::Error> {
        match self.device.detach_kernel_driver(self.number) {
            // ENODATA: nothing was bound, so there is nothing to detach.
            Err(e) if e.os_error() == Some(61) => Ok(()),
            other => other,
        }
    }

    fn claim(&mut self) -> Result<Interface, nusb::Error> {
        let interface = self.device.claim_interface(self.number).wait()?;
        self.interface = Some(interface.clone());
        Ok(interface)
    }
}

fn claim_error(error: &nusb::Error) -> ClaimError {
    match error.kind() {
        ErrorKind::Disconnected => ClaimError::Gone,
        _ => ClaimError::Failed { reason: error.to_string() },
    }
}

impl Claim for NusbClaim {
    fn release(&mut self) -> Result<(), ClaimError> {
        self.interface = None;
        Ok(())
    }

    fn reattach(&mut self) -> Result<(), ClaimError> {
        // While the last reference to the claim is still being dropped the kernel answers "busy".
        for attempt in 1..=REATTACH_TRIES {
            match self.device.attach_kernel_driver(self.number) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == ErrorKind::Busy && attempt < REATTACH_TRIES => thread::sleep(REATTACH_PAUSE),
                Err(e) if e.kind() == ErrorKind::Busy => {
                    return Err(ClaimError::Failed {
                        reason: format!("the interface was still busy after {REATTACH_TRIES} attempts"),
                    });
                },
                Err(e) => return Err(claim_error(&e)),
            }
        }
        Ok(())
    }
}

/// Opens the real device with the configured transfer counts and sizes.
pub struct RealUsb {
    config: UsbConfig,
}

impl RealUsb {
    pub const fn new(config: UsbConfig) -> Self {
        Self { config }
    }
}

impl UsbOpener for RealUsb {
    fn open(&mut self) -> Result<Link, UsbError> {
        let info = nusb::list_devices()
            .wait()
            .context(ListSnafu)?
            .find(|d| d.vendor_id() == VENDOR_ID && d.product_id() == PRODUCT_ID)
            .context(AbsentSnafu)?;
        let node = format!("/dev/bus/usb/{:03}/{:03}", info.busnum(), info.device_address());
        let device = info.open().wait().map_err(|source| match source.kind() {
            ErrorKind::PermissionDenied => UsbError::Denied { node: node.clone() },
            _ => UsbError::Open { node: node.clone(), source },
        })?;
        let endpoints = discover(&device.active_configuration().context(ConfigurationSnafu)?, INTERFACE)?;

        let mut guard = ClaimGuard::new(NusbClaim::new(device, endpoints.interface), RECOVERY);
        guard.claim_mut().detach().context(DetachSnafu { interface: endpoints.interface })?;
        let interface = guard.claim_mut().claim().context(ClaimInterfaceSnafu { interface: endpoints.interface })?;
        let address = endpoints.bulk_in;
        let reader = interface
            .endpoint::<Bulk, In>(address)
            .context(OpenEndpointSnafu { address })?
            .reader(self.config.in_size)
            .with_num_transfers(self.config.in_transfers)
            .with_read_timeout(IDLE_SLICE);
        let address = endpoints.bulk_out;
        let writer = interface
            .endpoint::<Bulk, Out>(address)
            .context(OpenEndpointSnafu { address })?
            .writer(self.config.out_size)
            .with_num_transfers(self.config.out_transfers)
            .with_write_timeout(WRITE_STALL);
        drop(interface);

        let detail = format!(
            "bus {} address {}, in {:#04x}, out {:#04x}, {}",
            info.busnum(),
            info.device_address(),
            endpoints.bulk_in,
            endpoints.bulk_out,
            self.config
        );
        Ok(Link::new(Transport::Usb, detail, UsbLink::new(Duplex::new(reader, writer), guard)))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use assert2::{assert, check};
    use rstest::rstest;

    use super::*;

    const INTERFACE_DESC: [u8; 9] = [9, 4, 0, 0, 2, 0xff, 0xff, 0xff, 0];

    fn endpoint(address: u8, attributes: u8) -> [u8; 7] {
        [7, 5, address, attributes, 0x00, 0x04, 0]
    }

    fn config_with(parts: &[&[u8]]) -> Vec<u8> {
        let body: Vec<u8> = parts.iter().flat_map(|p| p.iter().copied()).collect();
        let total = (9 + body.len()) as u16;
        let mut bytes = vec![9, 2, total as u8, (total >> 8) as u8, 1, 1, 0, 0x80, 50];
        bytes.extend(body);
        bytes
    }

    fn discovered(bytes: &[u8], interface: u8) -> Result<Endpoints, UsbError> {
        let config = ConfigurationDescriptor::new(bytes).expect("fixture is a configuration descriptor");
        discover(&config, interface)
    }

    #[test]
    fn endpoints_are_read_from_the_descriptors_not_assumed() {
        let bytes = config_with(&[&INTERFACE_DESC, &endpoint(0x03, 2), &endpoint(0x84, 2)]);
        check!(discovered(&bytes, 0).ok() == Some(Endpoints { interface: 0, bulk_in: 0x84, bulk_out: 0x03 }));
    }

    #[test]
    fn only_bulk_endpoints_of_the_default_alternate_setting_count() {
        let alt1 = [9, 4, 0, 1, 2, 0xff, 0xff, 0xff, 0];
        let bytes = config_with(&[
            &INTERFACE_DESC,
            &endpoint(0x85, 3),
            &endpoint(0x81, 2),
            &endpoint(0x01, 2),
            &alt1,
            &endpoint(0x82, 2),
            &endpoint(0x02, 2),
        ]);
        check!(discovered(&bytes, 0).ok() == Some(Endpoints { interface: 0, bulk_in: 0x81, bulk_out: 0x01 }));
    }

    #[rstest]
    #[case::no_out(&[&INTERFACE_DESC[..], &endpoint(0x81, 2)], 0, "interface 0 has no bulk OUT endpoint")]
    #[case::no_in(&[&INTERFACE_DESC[..], &endpoint(0x01, 2)], 0, "interface 0 has no bulk IN endpoint")]
    #[case::interrupt_only(&[&INTERFACE_DESC[..], &endpoint(0x81, 3), &endpoint(0x01, 3)], 0, "interface 0 has no bulk IN endpoint")]
    #[case::other_interface(&[&INTERFACE_DESC[..], &endpoint(0x81, 2), &endpoint(0x01, 2)], 1, "interface 1 has no alternate setting 0")]
    fn an_unusable_interface_says_what_is_missing(#[case] parts: &[&[u8]], #[case] interface: u8, #[case] want: &str) {
        let bytes = config_with(parts);
        assert!(let Err(e) = discovered(&bytes, interface));
        check!(e.to_string() == want);
    }

    #[test]
    fn the_permission_help_carries_the_exact_rule_and_the_fallback() {
        let help = permission_help("/dev/bus/usb/003/012");
        check!(help[0] == "--- cannot open /dev/bus/usb/003/012: permission denied ---");
        check!(help[2] == r#"SUBSYSTEM=="usb", ATTR{idVendor}=="1d6b", ATTR{idProduct}=="0010", TAG+="uaccess""#);
        check!(help.last().map(String::as_str) == Some("--- falling back to the tty transport ---"));
    }

    type Log = Rc<RefCell<Vec<String>>>;

    struct FakeIn {
        log: Log,
        data: Vec<u8>,
        fail: Option<io::ErrorKind>,
    }

    impl Read for FakeIn {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.log.borrow_mut().push("read".into());
            if let Some(kind) = self.fail {
                return Err(kind.into());
            }
            let n = buf.len().min(self.data.len());
            buf[..n].copy_from_slice(&self.data[..n]);
            self.data.drain(..n);
            Ok(n)
        }
    }

    impl Inbound for FakeIn {
        fn quiesce(&mut self) {
            self.log.borrow_mut().push("quiesce in".into());
        }
    }

    const PACKET: usize = 1024;
    const TRANSFER: usize = 64 * 1024;

    /// Buffers into 64 KiB transfers and submits a full one by itself, as nusb's writer does.
    struct FakeOut {
        log: Log,
        buffered: usize,
    }

    impl Write for FakeOut {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let n = buf.len().min(TRANSFER - self.buffered);
            self.buffered += n;
            if self.buffered == TRANSFER {
                self.buffered = 0;
                self.log.borrow_mut().push("full".into());
            }
            Ok(n)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Outbound for FakeOut {
        fn submit_end(&mut self) {
            let sent = std::mem::take(&mut self.buffered);
            if sent > 0 {
                self.log.borrow_mut().push(format!("send {sent}"));
            }
            if sent.is_multiple_of(PACKET) {
                self.log.borrow_mut().push("zlp".into());
            }
        }

        fn quiesce(&mut self) {
            self.log.borrow_mut().push("quiesce out".into());
        }
    }

    struct FakeClaim(Log);

    impl Claim for FakeClaim {
        fn release(&mut self) -> Result<(), ClaimError> {
            self.0.borrow_mut().push("release".into());
            Ok(())
        }

        fn reattach(&mut self) -> Result<(), ClaimError> {
            self.0.borrow_mut().push("reattach".into());
            Ok(())
        }
    }

    fn duplex(log: &Log, fail: Option<io::ErrorKind>) -> Duplex<FakeIn, FakeOut> {
        Duplex::new(
            FakeIn { log: Rc::clone(log), data: b"reply".to_vec(), fail },
            FakeOut { log: Rc::clone(log), buffered: 0 },
        )
    }

    #[rstest]
    #[case::short(5, &["send 5", "read"])]
    #[case::one_packet(PACKET, &["send 1024", "zlp", "read"])]
    #[case::whole_transfer(TRANSFER, &["full", "zlp", "read"])]
    #[case::transfer_and_a_few_bytes(TRANSFER + 5, &["full", "send 5", "read"])]
    #[case::transfer_and_a_packet(TRANSFER + PACKET, &["full", "send 1024", "zlp", "read"])]
    fn a_burst_ends_with_a_short_or_zero_length_packet_before_the_next_read(#[case] len: usize, #[case] want: &[&str]) {
        let log = Log::default();
        let mut link = duplex(&log, None);
        assert!(let Ok(()) = link.write_all(&vec![7; len]));
        check!(log.borrow().iter().all(|e| e != "read" && e != "zlp"), "a write alone ends nothing");
        let mut buf = [0; 8];
        check!(link.read(&mut buf).ok() == Some(5));
        check!(*log.borrow() == want);
    }

    #[test]
    fn nothing_is_submitted_when_nothing_was_written_and_a_flushed_burst_is_not_ended_twice() {
        let log = Log::default();
        let mut link = duplex(&log, None);
        let mut buf = [0; 8];
        drop(link.read(&mut buf));
        check!(*log.borrow() == ["read"], "an idle read sends no zero-length packet");
        assert!(let Ok(()) = link.write_all(b"abc"));
        assert!(let Ok(()) = link.flush());
        drop(link.read(&mut buf));
        check!(*log.borrow() == ["read", "send 3", "read"]);
    }

    #[rstest]
    #[case::unplugged(io::ErrorKind::ConnectionAborted, io::ErrorKind::BrokenPipe)]
    #[case::not_connected(io::ErrorKind::NotConnected, io::ErrorKind::BrokenPipe)]
    #[case::idle(io::ErrorKind::TimedOut, io::ErrorKind::TimedOut)]
    #[case::cancelled(io::ErrorKind::Interrupted, io::ErrorKind::Interrupted)]
    #[case::stall(io::ErrorKind::ConnectionReset, io::ErrorKind::ConnectionReset)]
    fn transfer_errors_map_onto_the_link_contract(#[case] from: io::ErrorKind, #[case] to: io::ErrorKind) {
        let log = Log::default();
        assert!(let Err(e) = duplex(&log, Some(from)).read(&mut [0; 4]));
        check!(e.kind() == to);
    }

    #[test]
    fn a_zero_length_terminator_is_idle_not_a_disconnect() {
        let log = Log::default();
        let mut link = duplex(&log, None);
        link.reader.data.clear();
        assert!(let Err(e) = link.read(&mut [0; 4]));
        check!(e.kind() == io::ErrorKind::TimedOut);
    }

    #[test]
    fn dropping_stops_the_endpoints_before_the_interface_is_handed_back() {
        let log = Log::default();
        let claim = ClaimGuard::new(FakeClaim(Rc::clone(&log)), "recover");
        drop(UsbLink::new(duplex(&log, None), claim));
        check!(*log.borrow() == ["quiesce out", "quiesce in", "release", "reattach"]);
    }
}
