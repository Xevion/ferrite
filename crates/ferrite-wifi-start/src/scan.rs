//! A scan through nl80211, as userspace runs one: the client in `ferrite-nl80211` over in-process
//! netlink sockets (`lx_nl_*` in the shadow `net/socket.c`), whose receives sleep on the socket's wait
//! queue on the shim scheduler.

use alloc::string::String;
use alloc::vec::Vec;
use core::ffi::{c_int, c_uint, c_void};
use core::time::Duration;

use ferrite_hal::Instant;
use ferrite_nl80211::client::{Client, ClientError, ScanEnd, Socket};
use ferrite_nl80211::genl::GroupId;
use ferrite_nl80211::netlink::Errno;
use ferrite_nl80211::nl80211::{Bss, Frequency, IfType, Interface, SsidDisplay};
use snafu::Snafu;

const NETLINK_GENERIC: c_int = 16;
const EAGAIN: c_int = 11;
const EMSGSIZE: c_int = 90;
const MSG_TRUNC: c_int = 0x20;

unsafe extern "C" {
    fn lx_nl_open(protocol: c_int, res: *mut *mut c_void) -> c_int;
    fn lx_nl_join(sock: *mut c_void, group: u32) -> c_int;
    fn lx_nl_send(sock: *mut c_void, buf: *const u8, len: usize) -> c_int;
    fn lx_nl_recv(sock: *mut c_void, buf: *mut u8, len: usize, timeout_ms: c_uint, flags: *mut c_int) -> c_int;
    fn lx_nl_close(sock: *mut c_void);
}

/// A `NETLINK_GENERIC` socket in `init_net`, closed on drop. Used only from kernel tasks.
pub struct KernelSocket(*mut c_void);

impl KernelSocket {
    pub fn open() -> Result<Self, Errno> {
        let mut sock = core::ptr::null_mut();
        // SAFETY: called on a task after the initcalls registered PF_NETLINK and generic netlink.
        let code = unsafe { lx_nl_open(NETLINK_GENERIC, &raw mut sock) };
        if code < 0 || sock.is_null() { Err(Errno(code)) } else { Ok(Self(sock)) }
    }
}

impl Drop for KernelSocket {
    fn drop(&mut self) {
        // SAFETY: the socket came from lx_nl_open and is released once.
        unsafe { lx_nl_close(self.0) };
    }
}

impl Socket for KernelSocket {
    fn send(&mut self, message: &[u8]) -> Result<(), Errno> {
        // SAFETY: a live socket; the bytes are read during the call only.
        let code = unsafe { lx_nl_send(self.0, message.as_ptr(), message.len()) };
        if code < 0 { Err(Errno(code)) } else { Ok(()) }
    }

    fn recv(&mut self, buf: &mut [u8], timeout: Duration) -> Result<Option<usize>, Errno> {
        let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX).max(1);
        let mut flags = 0;
        // SAFETY: a live socket; `buf` is writable for its length during the call.
        let code = unsafe { lx_nl_recv(self.0, buf.as_mut_ptr(), buf.len(), ms, &raw mut flags) };
        if code == -EAGAIN {
            return Ok(None);
        }
        let len = usize::try_from(code).map_err(|_| Errno(code))?;
        if flags & MSG_TRUNC != 0 || len > buf.len() {
            log::error!("a netlink datagram of {len} bytes did not fit the {}-byte buffer", buf.len());
            return Err(Errno(-EMSGSIZE));
        }
        Ok(Some(len))
    }

    fn join(&mut self, group: GroupId) -> Result<(), Errno> {
        // SAFETY: a live socket.
        let code = unsafe { lx_nl_join(self.0, group.raw()) };
        if code < 0 { Err(Errno(code)) } else { Ok(()) }
    }
}

#[derive(Debug, Snafu)]
pub enum ScanError {
    #[snafu(display("opening a generic netlink socket failed: {errno}"))]
    Open { errno: Errno },
    #[snafu(display("{source}"))]
    Nl80211 { source: ClientError },
    #[snafu(display("nl80211 lists no interface named {name}"))]
    NoInterface { name: String },
    #[snafu(display("the scan on {name} was aborted after {} ms", waited.as_millis()))]
    Aborted { name: String, waited: Duration },
    #[snafu(display("the scan on {name} did not end within {} ms", limit.as_millis()))]
    Expired { name: String, limit: Duration },
    #[snafu(display("the scan on {name} was cancelled"))]
    Cancelled { name: String },
    #[snafu(display("creating a kthread failed: {code}"))]
    Thread { code: isize },
}

impl From<ClientError> for ScanError {
    fn from(source: ClientError) -> Self {
        Self::Nl80211 { source }
    }
}

/// An nl80211 client over two fresh kernel sockets.
pub fn connect() -> Result<Client<KernelSocket>, ScanError> {
    let open = || KernelSocket::open().map_err(|errno| ScanError::Open { errno });
    Ok(Client::connect(open()?, open()?)?)
}

/// What one scan found.
pub struct ScanReport {
    pub bsses: Vec<Bss>,
    pub took: Duration,
}

/// How often a waiting scan looks at its cancel flag.
const CANCEL_SLICE: Duration = Duration::from_millis(100);

/// Trigger a scan of `name` (which must be up), wait for its end on the scan group for at most
/// `limit` (or until `cancel` says so, checked every `CANCEL_SLICE`), then dump every BSS and log each
/// at `bss_level`. The scan covers `frequencies` (every channel when empty) and probes for each `hidden`
/// name directly, besides the usual scan.
pub fn scan(
    client: &mut Client<KernelSocket>,
    name: &str,
    (frequencies, hidden): (&[Frequency], &[&[u8]]),
    limit: Duration,
    cancel: &dyn Fn() -> bool,
    bss_level: log::Level,
    clock: &dyn Fn() -> Instant,
) -> Result<ScanReport, ScanError> {
    let interface = find(client, name)?;
    let start = clock();
    client.trigger_scan(interface.ifindex, frequencies, hidden)?;
    log::info!("nl80211: scan triggered on {name} (ifindex {}, wiphy {})", interface.ifindex, interface.wiphy);
    let deadline = start + limit;
    let end = loop {
        let slice = (clock() + CANCEL_SLICE).min(deadline);
        match client.wait_scan(interface.ifindex, slice, clock)? {
            Some(end) => break Some(end),
            None if cancel() => return Err(ScanError::Cancelled { name: name.into() }),
            None if clock() >= deadline => break None,
            None => {},
        }
    };
    match end {
        Some(ScanEnd::Done) => {},
        Some(ScanEnd::Aborted) => return Err(ScanError::Aborted { name: name.into(), waited: clock() - start }),
        None => return Err(ScanError::Expired { name: name.into(), limit }),
    }
    let took = clock() - start;
    let bsses = client.scan_results(interface.ifindex)?;
    for bss in &bsses {
        let signal = bss.signal.map_or_else(|| String::from("?"), |s| alloc::format!("{s}"));
        log::log!(
            bss_level,
            "  BSS {} {} MHz {signal} {} \"{}\"",
            bss.bssid,
            bss.frequency.0,
            bss.security,
            SsidDisplay(&bss.ssid)
        );
    }
    log::info!("nl80211: scan on {name} done after {} ms: {} BSS", took.as_millis(), bsses.len());
    Ok(ScanReport { bsses, took })
}

pub fn find(client: &mut Client<KernelSocket>, name: &str) -> Result<Interface, ScanError> {
    client
        .interfaces()?
        .into_iter()
        .find(|interface| interface.name == name)
        .ok_or_else(|| ScanError::NoInterface { name: name.into() })
}

/// Make `name` an AP interface and bring it up between the type change and the start, as hostapd's
/// interface setup does; `up` brings it up.
pub fn prepare_ap(
    client: &mut Client<KernelSocket>,
    name: &str,
    up: impl FnOnce() -> Result<(), c_int>,
) -> Result<Interface, ScanError> {
    let interface = find(client, name)?;
    client.set_iftype(interface.ifindex, IfType::Ap)?;
    up().map_err(|code| ScanError::Nl80211 {
        source: ClientError::Kernel { what: "dev_open of the AP", errno: Errno(code), text: None },
    })?;
    Ok(interface)
}
