//! The frame seam between a Linux `net_device` and the program's smoltcp, and the hand-off of an open
//! link from the init task to the program's main loop.
//!
//! `glue/netif.c` registers an `rx_handler` that copies each received frame into the [`Link`]'s queue
//! and consumes the skb, and runs a transmit kthread that sends the link's queued frames with
//! `dev_queue_xmit`, woken when smoltcp queues one. Linux code runs only on tasks: the main loop only
//! touches the queues and marks the thread runnable.
//!
//! Once the port is open, init attaches the link and publishes a [`Session`]; the main loop takes it
//! with [`take_session`], runs IP over it, and reports with [`finish_session`]. Init holds the
//! connection until then (or its own bound), detaches the link and disconnects.

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::String;
use core::cell::RefCell;
use core::ffi::{c_int, c_uint, c_void};
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU8, Ordering};

use ferrite_netif::{FRAME_MAX, Link};
use ferrite_nl80211::nl80211::IfIndex;

/// Frames queued in each direction before more are dropped.
const RX_QUEUE: usize = 64;
const TX_QUEUE: usize = 64;

unsafe extern "C" {
    fn lx_netif_lookup(ifindex: c_int, mac: *mut u8, mtu: *mut c_uint) -> c_int;
    fn lx_netif_attach(
        ifindex: c_int,
        ctx: *const c_void,
        rx: unsafe extern "C" fn(*const c_void, *const u8, c_uint),
        pull: unsafe extern "C" fn(*const c_void, *mut u8, c_uint) -> c_int,
        sent: unsafe extern "C" fn(*const c_void, c_int),
        res: *mut *mut c_void,
    ) -> c_int;
    fn lx_netif_kick(netif: *mut c_void);
    fn lx_netif_detach(netif: *mut c_void);
}

unsafe extern "C" fn rx(ctx: *const c_void, frame: *const u8, len: c_uint) {
    // SAFETY: `ctx` is the `Link` of a live attachment, which detaches before the link goes.
    let link = unsafe { &*ctx.cast::<Link>() };
    if frame.is_null() {
        link.count_rx_drop();
    } else {
        // SAFETY: the handler passes its copy of the frame, `len` bytes, for the call's duration.
        link.deliver(unsafe { core::slice::from_raw_parts(frame, len as usize) });
    }
}

unsafe extern "C" fn pull(ctx: *const c_void, buf: *mut u8, cap: c_uint) -> c_int {
    // SAFETY: as in `rx`.
    let link = unsafe { &*ctx.cast::<Link>() };
    let Some(frame) = link.next_tx() else { return 0 };
    if frame.len() > cap as usize {
        link.count_tx_drop();
        return -1;
    }
    // SAFETY: `buf` has `cap` writable bytes, at least the frame's length.
    unsafe { core::ptr::copy_nonoverlapping(frame.as_ptr(), buf, frame.len()) };
    c_int::try_from(frame.len()).unwrap_or(-1)
}

unsafe extern "C" fn sent(ctx: *const c_void, code: c_int) {
    // SAFETY: as in `rx`.
    let link = unsafe { &*ctx.cast::<Link>() };
    link.transmitted(code == 0);
}

/// A `net_device`'s frames routed to a [`Link`] until [`Attachment::detach`].
pub struct Attachment {
    link: Rc<Link>,
    netif: NonNull<c_void>,
    ifindex: IfIndex,
}

/// Attach to `ifindex` from a task: its address and MTU, the `rx_handler` and the transmit thread.
pub fn attach(ifindex: IfIndex) -> Result<Attachment, c_int> {
    let raw = ifindex.raw().cast_signed();
    let (mut mac, mut mtu) = ([0_u8; 6], 0);
    // SAFETY: called on a task; both out-pointers are writable.
    let code = unsafe { lx_netif_lookup(raw, mac.as_mut_ptr(), &raw mut mtu) };
    if code != 0 {
        return Err(code);
    }
    let link = Rc::new(Link::new(mac, mtu as usize, RX_QUEUE, TX_QUEUE));
    let mut netif = core::ptr::null_mut();
    // SAFETY: called on a task; the link outlives the attachment, which `detach` ends.
    let code = unsafe { lx_netif_attach(raw, Rc::as_ptr(&link).cast(), rx, pull, sent, &raw mut netif) };
    let Some(netif) = NonNull::new(netif).filter(|_| code == 0) else { return Err(code) };
    let kick = netif.as_ptr();
    // SAFETY: the kick is cleared in `detach` before the thread it wakes is stopped.
    link.set_kick(Some(Box::new(move || unsafe { lx_netif_kick(kick) })));
    log::info!(
        "net: frames of ifindex {ifindex} ({}, MTU {mtu}) go to the IP stack; at most {RX_QUEUE} queued each way, frames up to {FRAME_MAX} bytes",
        ferrite_nl80211::nl80211::MacAddr(mac)
    );
    Ok(Attachment { link, netif, ifindex })
}

impl Attachment {
    pub fn link(&self) -> Rc<Link> {
        Rc::clone(&self.link)
    }

    /// Stop routing frames, from a task, and log the link's counters.
    pub fn detach(self) {
        self.link.set_kick(None);
        // SAFETY: on a task; the attachment is live and ended once.
        unsafe { lx_netif_detach(self.netif.as_ptr()) };
        log::info!("net: ifindex {} detached; frames {}", self.ifindex, self.link.stats());
    }
}

/// An open link for the main loop: the station's, and in the lab the AP's too.
pub struct Session {
    pub station: Rc<Link>,
    pub station_name: String,
    pub ap: Option<Rc<Link>>,
}

const IDLE: u8 = 0;
const READY: u8 = 1;
const TAKEN: u8 = 2;
const FINISHED_OK: u8 = 3;
const FINISHED_FAILED: u8 = 4;
const CLOSED: u8 = 5;

static STATE: AtomicU8 = AtomicU8::new(IDLE);

struct Slot(RefCell<Option<Session>>);

// SAFETY: one CPU, and the tasks and the main loop never run at once; no borrow outlives a call here.
unsafe impl Sync for Slot {}

static SLOT: Slot = Slot(RefCell::new(None));

pub(crate) fn publish(session: Session) {
    if let Ok(mut slot) = SLOT.0.try_borrow_mut() {
        *slot = Some(session);
        STATE.store(READY, Ordering::Release);
    }
}

/// The open link, once, when init has published one.
pub fn take_session() -> Option<Session> {
    STATE.compare_exchange(READY, TAKEN, Ordering::AcqRel, Ordering::Acquire).ok()?;
    SLOT.0.try_borrow_mut().ok()?.take()
}

/// The main loop is done with the link; `ok` is whether its IP checks passed.
pub fn finish_session(ok: bool) {
    let _ = STATE.compare_exchange(
        TAKEN,
        if ok { FINISHED_OK } else { FINISHED_FAILED },
        Ordering::AcqRel,
        Ordering::Acquire,
    );
}

/// Init has detached the link, so the main loop's stack over it should go.
pub fn session_closed() -> bool {
    STATE.load(Ordering::Acquire) == CLOSED
}

/// What the main loop reported, once it has.
pub(crate) fn outcome() -> Option<bool> {
    match STATE.load(Ordering::Acquire) {
        FINISHED_OK => Some(true),
        FINISHED_FAILED => Some(false),
        _ => None,
    }
}

/// Whether the main loop took the session.
pub(crate) fn taken() -> bool {
    STATE.load(Ordering::Acquire) != READY
}

pub(crate) fn close() {
    if let Ok(mut slot) = SLOT.0.try_borrow_mut() {
        slot.take();
    }
    STATE.store(CLOSED, Ordering::Release);
}
