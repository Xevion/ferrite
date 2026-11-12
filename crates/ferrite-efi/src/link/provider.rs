//! The loader's side of the link: the installed protocol, the timer that keeps the transport
//! serviced, and the one shared state behind both.
//!
//! The firmware hands the protocol's `this` pointer and the timer's context to code that runs
//! outside any Rust borrow, so the state they reach is leaked from a `Box` and referred to by raw
//! pointer until [`Provider::close`] or its `Drop` frees it. Exclusion is two layers: the task
//! priority level keeps the timer notify (at `TPL_CALLBACK`) from running inside any access made
//! at that level, and the `RefCell`s turn the one remaining case, re-entry from the same context,
//! into a refusal instead of aliasing.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::cell::{RefCell, RefMut};
use core::ffi::c_void;
use core::fmt;
use core::ptr::NonNull;
use core::slice;
use core::time::Duration;

use ferrite_wire::link::{Message, ShortStr, Stream};
use ferrite_xhci::State as DbcState;
use snafu::{ResultExt, Snafu};
use uefi::boot::{self, EventType, TimerTrigger, Tpl, TplGuard};
use uefi::{Event, Handle, Status};

use super::hub::{Hub, HubStats, Mode, SendError, Transport, frame_output};
use super::{GUID, LinkProtocol, REVISION};
use crate::session;
use crate::xhci::{EfiDbc, OpenDbc};

/// How often the timer pumps the hub.
const TICK: Duration = Duration::from_millis(1);
/// Encoded bytes that can wait while the hub is busy.
const PARK_CAP: usize = 64 * 1024;

impl Transport for OpenDbc {
    fn poll_rx(&mut self, rx: impl FnMut(&[u8])) {
        EfiDbc::poll(self, rx);
    }

    fn send(&mut self, data: &[u8]) -> usize {
        EfiDbc::write(self, data).accepted
    }

    fn is_up(&self) -> bool {
        self.state() == DbcState::Configured
    }

    fn status(&self) -> &'static str {
        self.state().into()
    }

    fn backlog(&self) -> usize {
        self.buffered()
    }
}

/// Everything the protocol functions, the timer and the loader share.
struct State<T> {
    hub: RefCell<Hub<T>>,
    /// Frames from writes that found `hub` busy, in order; absorbed by the next hub access.
    parked: RefCell<Vec<u8>>,
}

/// The protocol table with its state behind it. `proto` is first so a pointer to one is a
/// pointer to the other.
#[repr(C)]
struct Instance<T> {
    proto: LinkProtocol,
    state: State<T>,
}

fn at_callback() -> TplGuard {
    // SAFETY: raising the level only defers lower-priority notifications until the guard drops,
    // and nothing done while it is held waits on an event.
    unsafe { boot::raise_tpl(Tpl::CALLBACK) }
}

impl<T: Transport> State<T> {
    /// The hub, with any parked frames absorbed first. `None` when the hub is already borrowed
    /// further up this call chain.
    fn lock(&self) -> Option<RefMut<'_, Hub<T>>> {
        let mut hub = self.hub.try_borrow_mut().ok()?;
        if let Ok(mut parked) = self.parked.try_borrow_mut() {
            hub.absorb(&parked);
            parked.clear();
        }
        Some(hub)
    }

    fn with_hub<R>(&self, f: impl FnOnce(&mut Hub<T>) -> R) -> Option<R> {
        let _level = at_callback();
        let mut hub = self.lock()?;
        Some(f(&mut hub))
    }

    fn pump(&self) {
        let reboot = self
            .with_hub(|hub| {
                hub.pump();
                hub.reboot_requested()
            })
            .unwrap_or(false);
        if reboot {
            log::info!("the desktop asked for a reboot");
            session::reboot();
        }
    }

    fn write(&self, stream: Stream, bytes: &[u8]) -> Status {
        let _level = at_callback();
        let Some(mut hub) = self.lock() else {
            return self.park(stream, bytes);
        };
        let mut queued = hub.output(stream, bytes);
        if queued.is_err() {
            // Completions free queue space only when the transport is serviced, which a writer
            // in a tight loop would otherwise leave to the timer.
            hub.pump();
            queued = hub.output(stream, bytes);
        }
        hub.flush();
        if queued.is_ok() { Status::SUCCESS } else { Status::OUT_OF_RESOURCES }
    }

    fn park(&self, stream: Stream, bytes: &[u8]) -> Status {
        let Ok(mut parked) = self.parked.try_borrow_mut() else {
            return Status::OUT_OF_RESOURCES;
        };
        let room = PARK_CAP.saturating_sub(parked.len());
        if frame_output(stream, bytes, room, &mut parked).is_ok() { Status::SUCCESS } else { Status::OUT_OF_RESOURCES }
    }

    fn read(&self, buf: &mut [u8]) -> usize {
        self.with_hub(|hub| hub.read_input(buf)).unwrap_or(0)
    }
}

/// The state behind a protocol table.
///
/// # Safety
///
/// `this` must be null or a table the provider installed that is still installed.
unsafe fn state_of<'a, T>(this: *const LinkProtocol) -> Option<&'a State<T>> {
    // SAFETY: per the contract, `this` is a live table.
    let context = unsafe { this.as_ref() }?.context;
    // SAFETY: `context` points at the `state` field of the same live `Instance`.
    unsafe { context.cast::<State<T>>().as_ref() }
}

unsafe extern "efiapi" fn proto_write<T: Transport>(
    this: *const LinkProtocol,
    stream: u32,
    buf: *const u8,
    len: usize,
) -> Status {
    // SAFETY: the firmware passes back the pointer the provider installed.
    let Some(state) = (unsafe { state_of::<T>(this) }) else {
        return Status::INVALID_PARAMETER;
    };
    let Ok(stream @ (Stream::Log | Stream::Data)) = u8::try_from(stream).map(Stream::from) else {
        return Status::INVALID_PARAMETER;
    };
    if len == 0 {
        return Status::SUCCESS;
    }
    if buf.is_null() {
        return Status::INVALID_PARAMETER;
    }
    // SAFETY: the caller guarantees `buf` is readable for `len` bytes.
    state.write(stream, unsafe { slice::from_raw_parts(buf, len) })
}

unsafe extern "efiapi" fn proto_read<T: Transport>(
    this: *const LinkProtocol,
    buf: *mut u8,
    cap: usize,
    len: *mut usize,
) -> Status {
    // SAFETY: as `proto_write`.
    let Some(state) = (unsafe { state_of::<T>(this) }) else {
        return Status::INVALID_PARAMETER;
    };
    if len.is_null() || (buf.is_null() && cap != 0) {
        return Status::INVALID_PARAMETER;
    }
    let got = if cap == 0 {
        0
    } else {
        // SAFETY: the caller guarantees `buf` is writable for `cap` bytes.
        state.read(unsafe { slice::from_raw_parts_mut(buf, cap) })
    };
    // SAFETY: `len` is non-null and the caller guarantees it is writable.
    unsafe { len.write(got) };
    Status::SUCCESS
}

unsafe extern "efiapi" fn proto_poll<T: Transport>(this: *const LinkProtocol) -> Status {
    // SAFETY: as `proto_write`.
    let Some(state) = (unsafe { state_of::<T>(this) }) else {
        return Status::INVALID_PARAMETER;
    };
    state.pump();
    Status::SUCCESS
}

unsafe extern "efiapi" fn tick<T: Transport>(_event: Event, context: Option<NonNull<c_void>>) {
    let Some(context) = context else {
        return;
    };
    // SAFETY: `context` is the `State` the timer was created with, and the event is closed
    // before the state is freed.
    unsafe { context.cast::<State<T>>().as_ref() }.pump();
}

#[derive(Debug, Snafu)]
pub enum InstallError {
    #[snafu(display("installing the link protocol failed"))]
    Install { source: uefi::Error },
    #[snafu(display("starting the link timer failed"))]
    Timer { source: uefi::Error },
}

/// A provider that could not be closed, handed back intact.
pub struct Retained<T: Transport> {
    pub provider: Provider<T>,
    pub source: uefi::Error,
}

impl<T: Transport> fmt::Debug for Retained<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Retained").field("source", &self.source).finish_non_exhaustive()
    }
}

impl<T: Transport> fmt::Display for Retained<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "uninstalling the link protocol failed: {}", self.source)
    }
}

/// The installed link: owns the DbC, the protocol and the timer.
pub struct Provider<T: Transport> {
    inst: NonNull<Instance<T>>,
    handle: Handle,
    timer: Option<Event>,
    /// The instance is installed and not yet freed.
    live: bool,
}

impl<T: Transport> fmt::Debug for Provider<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Provider").field("live", &self.live).finish_non_exhaustive()
    }
}

impl<T: Transport> Provider<T> {
    /// Take over `transport`, install the protocol and start the timer. `build` is announced in `Ready`.
    ///
    /// On failure the transport is dropped with everything else.
    pub fn install(transport: T, build: ShortStr) -> Result<Self, InstallError> {
        let instance = Box::new(Instance::<T> {
            proto: LinkProtocol {
                revision: REVISION,
                write: proto_write::<T>,
                read: proto_read::<T>,
                poll: proto_poll::<T>,
                context: core::ptr::null(),
            },
            state: State { hub: RefCell::new(Hub::new(transport, build)), parked: RefCell::new(Vec::new()) },
        });
        let inst = NonNull::from(Box::leak(instance));
        // SAFETY: `inst` is the only reference to the leaked instance until it is installed.
        unsafe { (*inst.as_ptr()).proto.context = (&raw const (*inst.as_ptr()).state).cast() };

        // SAFETY: `GUID` is the protocol's own, and the interface stays valid until `teardown`.
        let handle = unsafe { boot::install_protocol_interface(None, &GUID, inst.as_ptr().cast_const().cast()) };
        let handle = match handle.context(InstallSnafu) {
            Ok(handle) => handle,
            Err(e) => {
                // SAFETY: nothing else saw the instance.
                drop(unsafe { Box::from_raw(inst.as_ptr()) });
                return Err(e);
            },
        };

        let mut provider = Self { inst, handle, timer: None, live: true };
        // SAFETY: the notify only touches `State`, and `teardown` closes the event before the
        // state is freed; boot services stay up because the loader never exits them.
        let created = unsafe {
            boot::create_event(
                EventType::TIMER | EventType::NOTIFY_SIGNAL,
                Tpl::CALLBACK,
                Some(tick::<T>),
                NonNull::new(&raw mut (*inst.as_ptr()).state).map(NonNull::cast),
            )
        };
        let event = provider.timer.insert(created.context(TimerSnafu)?);
        boot::set_timer(event, TimerTrigger::Periodic(TICK)).context(TimerSnafu)?;
        Ok(provider)
    }

    fn state(&self) -> &State<T> {
        // SAFETY: the instance lives until `teardown`, which needs `&mut self` or consumes it.
        unsafe { &(*self.inst.as_ptr()).state }
    }

    /// Service the DbC and route what arrived. Acts on a `Reboot` request.
    pub fn pump(&self) {
        self.state().pump();
    }

    /// Queue a message for the desktop.
    pub fn send(&self, msg: &Message) -> Result<(), SendError> {
        self.state().with_hub(|hub| hub.send(msg)).unwrap_or(Err(SendError::Full))
    }

    /// The oldest push or abort the desktop sent while no app was running.
    pub fn next_command(&self) -> Option<Message> {
        self.state().with_hub(Hub::next_command).flatten()
    }

    pub fn set_mode(&self, mode: Mode) {
        self.state().with_hub(|hub| hub.set_mode(mode));
    }

    pub fn stats(&self) -> Option<HubStats> {
        self.state().with_hub(|hub| hub.stats())
    }

    /// The transport's own name for its link state, such as `Configured` for a DbC.
    pub fn link_status(&self) -> Option<&'static str> {
        self.state().with_hub(|hub| hub.transport().status())
    }

    /// Encoded bytes not yet confirmed delivered.
    pub fn pending(&self) -> usize {
        self.state().with_hub(|hub| hub.pending()).unwrap_or(0)
    }

    /// Uninstall the protocol, stop the timer and give the transport back.
    ///
    /// If the firmware will not uninstall the protocol the provider is returned untouched and
    /// keeps running.
    pub fn close(mut self) -> Result<T, Retained<T>> {
        match self.teardown() {
            Ok(instance) => {
                let Instance { state, .. } = *instance;
                Ok(state.hub.into_inner().into_transport())
            },
            Err(source) => Err(Retained { provider: self, source }),
        }
    }

    /// Stop everything and free the instance, or leave it all running if the protocol stays.
    fn teardown(&mut self) -> uefi::Result<Box<Instance<T>>> {
        let _level = at_callback();
        if let Some(timer) = &self.timer {
            boot::set_timer(timer, TimerTrigger::Cancel)?;
        }
        // SAFETY: the pointer is the interface installed in `install`; clients hold it
        // untracked and are told it goes away.
        let uninstalled =
            unsafe { boot::uninstall_protocol_interface(self.handle, &GUID, self.inst.as_ptr().cast_const().cast()) };
        if let Err(e) = uninstalled {
            if let Some(timer) = &self.timer
                && let Err(rearm) = boot::set_timer(timer, TimerTrigger::Periodic(TICK))
            {
                log::error!("link timer did not restart after a failed uninstall: {rearm}");
            }
            return Err(e);
        }
        if let Some(timer) = self.timer.take()
            && let Err(e) = boot::close_event(timer)
        {
            log::warn!("closing the link timer failed: {e}");
        }
        self.live = false;
        // SAFETY: the protocol is gone and the timer is closed, so nothing can reach the
        // instance any more, and it came from `Box::leak` in `install`.
        Ok(unsafe { Box::from_raw(self.inst.as_ptr()) })
    }
}

impl<T: Transport> Drop for Provider<T> {
    fn drop(&mut self) {
        if !self.live {
            return;
        }
        match self.teardown() {
            Ok(instance) => drop(instance),
            Err(e) => log::error!("link protocol stays installed over leaked state: {e}"),
        }
    }
}
