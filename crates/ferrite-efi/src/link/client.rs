//! The app side of the link: find the loader's protocol and use it safely.

use core::ptr::NonNull;

use ferrite_wire::link::Stream;
use snafu::Snafu;
use uefi::Status;
use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams};

use super::{LinkProtocol, MIN_REVISION};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Snafu)]
pub enum Error {
    #[snafu(display("the link's output queue is full"))]
    Full,
    #[snafu(display("the link refused the call: {status}"))]
    Firmware { status: Status },
}

/// A loader is running and has installed the link protocol.
#[must_use]
pub fn is_present() -> bool {
    boot::find_handles::<LinkProtocol>().is_ok_and(|handles| !handles.is_empty())
}

/// A handle to the loader's link. Copies share the one link.
#[derive(Clone, Copy, Debug)]
pub struct Link {
    proto: NonNull<LinkProtocol>,
}

impl Link {
    /// The loader's link, or `None` when no loader runs or it speaks another revision.
    pub fn locate() -> Option<Self> {
        let handle = boot::get_handle_for_protocol::<LinkProtocol>().ok()?;
        let params = OpenProtocolParams { handle, agent: boot::image_handle(), controller: None };
        // SAFETY: the loader keeps the interface installed while any app it started runs, and
        // every function takes the table by shared pointer, so an untracked open conflicts with nothing.
        let scoped =
            unsafe { boot::open_protocol::<LinkProtocol>(params, OpenProtocolAttributes::GetProtocol) }.ok()?;
        let proto = NonNull::from(scoped.get()?);
        // An untracked open has nothing to close, and closing one would report an error.
        core::mem::forget(scoped);
        // SAFETY: `proto` was just obtained from the firmware and points at a live table.
        let revision = unsafe { proto.as_ref() }.revision;
        if revision < MIN_REVISION {
            log::warn!("link protocol revision {revision}, this build needs at least {MIN_REVISION}");
            return None;
        }
        Some(Self { proto })
    }

    fn table(&self) -> &LinkProtocol {
        // SAFETY: see `locate`: the table outlives every app the loader runs.
        unsafe { self.proto.as_ref() }
    }

    /// Queue `bytes` for the desktop on `stream`, all or nothing. Never blocks.
    pub fn write(&self, stream: Stream, bytes: &[u8]) -> Result<(), Error> {
        let table = self.table();
        // SAFETY: the table's function pointers are the provider's, called with its own table
        // pointer and a readable buffer of the stated length.
        let status = unsafe { (table.write)(table, u32::from(stream.code()), bytes.as_ptr(), bytes.len()) };
        match status {
            Status::SUCCESS => Ok(()),
            Status::OUT_OF_RESOURCES => Err(Error::Full),
            status => Err(Error::Firmware { status }),
        }
    }

    /// Copy waiting `Input` bytes into `buf` and return how many; 0 when there are none.
    pub fn read(&self, buf: &mut [u8]) -> Result<usize, Error> {
        let table = self.table();
        let mut len = 0usize;
        // SAFETY: as `write`, with a writable buffer of the stated capacity and a valid length slot.
        let status = unsafe { (table.read)(table, buf.as_mut_ptr(), buf.len(), &raw mut len) };
        match status {
            Status::SUCCESS => Ok(len.min(buf.len())),
            status => Err(Error::Firmware { status }),
        }
    }

    /// The desktop asked the running app to finish. False when the loader is too old to carry the
    /// request. The answer stays true until the app ends, so every caller sees it.
    #[must_use]
    pub fn stop_requested(&self) -> bool {
        let table = self.table();
        if table.revision < 2 {
            return false;
        }
        let mut requested = 0u8;
        // SAFETY: the table is revision 2 or later, so the function exists; `requested` is a valid slot.
        let status = unsafe { (table.stop_requested)(table, &raw mut requested) };
        status == Status::SUCCESS && requested != 0
    }

    /// Service the DbC now, for a loop that has been busy for a while.
    pub fn poll(&self) {
        let table = self.table();
        // SAFETY: as `write`.
        let _ = unsafe { (table.poll)(table) };
    }
}
