//! The link between the resident loader and the apps it runs.
//!
//! The loader keeps one DbC open for its whole life. Apps it starts reach the desktop through a
//! UEFI protocol the loader installs instead of opening the DbC a second time. [`LinkProtocol`] is
//! that protocol, [`Link`] is the safe client an app uses, [`logger`](crate::logger) sends every
//! log record through it when it is present, and [`session::finish`](crate::session::finish)
//! returns instead of resetting when it is. The loader's side is [`provider::Provider`]; the
//! framing, queues and routing live in [`hub`], and the push receiver in [`receive`]. The
//! messages themselves are [`ferrite_wire::link`].
//!
//! # Protocol
//!
//! GUID [`GUID`], revision [`REVISION`]. All functions are non-blocking and may be called at any
//! task priority level up to `TPL_CALLBACK`.
//!
//! | Function | Behaviour |
//! | --- | --- |
//! | `write(this, stream, buf, len)` | Frames `len` bytes as `Output` on `stream` (0 log, 1 data) and queues them for the desktop. `SUCCESS` when all of it was queued, `OUT_OF_RESOURCES` when the queue is full (nothing was queued), `INVALID_PARAMETER` for an unknown stream or a null `buf` with `len` > 0 |
//! | `read(this, buf, cap, len)` | Stores up to `cap` bytes the desktop sent as `Input` in `buf` and their count in `*len`; `SUCCESS` with 0 when nothing is waiting |
//! | `poll(this)` | Services the DbC now; always `SUCCESS` |
//! | `stop_requested(this, requested)` | Revision 2. Stores 1 in `*requested` once the desktop sent `Stop` for the running app, else 0; asking does not clear it, and it clears when the app ends |
//!
//! `Stop` is not part of `read`, whose contract is a byte stream: folding a control event into it
//! would either put a marker into the data or give existing callers a status they never handled.
//! A separate function is additive. Revision 2 appends it after the opaque field, so every
//! revision 1 offset is unchanged, and a client calls it only when the table's `revision` says it
//! is there ([`Link::locate`] accepts any revision from [`MIN_REVISION`] up).
//!
//! # Priority levels and the timer
//!
//! An app may never call into the protocol, yet the DbC must be serviced while it runs. The
//! provider therefore creates a periodic timer event (1 ms) with a notify function at
//! `TPL_CALLBACK` that pumps the hub. Every other entry into the hub, whether from a protocol
//! function called by an app or from the loader's own code, first raises the level to
//! `TPL_CALLBACK` for its duration. The timer notify cannot preempt code running at its own
//! level, so a pump and a protocol call never overlap. Firmware ticks timers at its own rate (every
//! 10 ms under OVMF), so an app that moves bulk data should call [`Link::poll`] in its loop; a
//! `write` that finds the queue full services the link once itself. The loader's idle loop pumps
//! directly, so image transfer does not depend on the tick.
//!
//! Re-entry is the one case a level cannot prevent: the DbC driver logs, the logger writes to the
//! link, and that write arrives while the pump holds the hub. A write finding the hub busy
//! encodes its frames into a separate small queue that the next hub access absorbs in order;
//! a read finding it busy reports nothing waiting.

use core::ffi::c_void;

use uefi::proto::unsafe_protocol;
use uefi::{Guid, Status, guid};

pub mod client;
pub mod hub;
pub mod provider;
pub mod receive;

pub use client::{Error, Link, is_present};
pub use ferrite_wire::link::Stream;

pub const GUID: Guid = guid!("e0591406-fec2-4e04-aebe-4890533243d2");
/// Revision of the table the provider installs.
pub const REVISION: u32 = 2;
/// The oldest revision a client can use.
pub const MIN_REVISION: u32 = 1;

/// The function table the loader installs. Apps use [`Link`] rather than calling it directly.
#[repr(C)]
#[derive(Debug)]
#[unsafe_protocol(GUID)]
pub struct LinkProtocol {
    pub revision: u32,
    pub write: unsafe extern "efiapi" fn(this: *const Self, stream: u32, buf: *const u8, len: usize) -> Status,
    pub read: unsafe extern "efiapi" fn(this: *const Self, buf: *mut u8, cap: usize, len: *mut usize) -> Status,
    pub poll: unsafe extern "efiapi" fn(this: *const Self) -> Status,
    /// The provider's state; opaque to every client.
    context: *const c_void,
    /// Present from revision 2.
    pub stop_requested: unsafe extern "efiapi" fn(this: *const Self, requested: *mut u8) -> Status,
}
