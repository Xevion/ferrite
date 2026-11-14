//! The frame seam between a Linux `net_device` and the program's smoltcp.
//!
//! `glue/netif.c` registers an `rx_handler` that copies each received frame into the [`QueuePort`]'s queue
//! and consumes the skb, and runs a transmit kthread that sends the link's queued frames with
//! `dev_queue_xmit`. Linux code runs only on tasks: when smoltcp queues a frame the main loop only
//! touches the queue and marks the sleeping thread runnable in the scheduler, never through the C.
//!
//! The station service and the test script attach a link once its port is open and offer the
//! [`QueuePort`] to the main loop through `ferrite_wifi::Shared`.

use alloc::boxed::Box;
use alloc::rc::Rc;
use core::ffi::{c_int, c_uint, c_void};
use core::ptr::NonNull;

use ferrite_net::QueuePort;
use ferrite_net::mdns::MDNS_GROUP;
use ferrite_net::port::{FRAME_MAX, MacAddr};
use ferrite_nl80211::nl80211::IfIndex;
use ferrite_wifi::Sleeper;

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
    fn lx_netif_join(netif: *mut c_void, mac: *const u8) -> c_int;
    fn lx_netif_tx_task(netif: *mut c_void) -> *mut c_void;
    fn lx_netif_detach(netif: *mut c_void);
}

/// What the C callbacks and the main loop's kick share for one attachment.
struct Seam {
    link: Rc<QueuePort>,
    /// Whether the transmit thread is parked waiting for a frame.
    sleeper: Sleeper,
}

unsafe extern "C" fn rx(ctx: *const c_void, frame: *const u8, len: c_uint) {
    // SAFETY: `ctx` is the `Seam` of a live attachment, which detaches before the seam goes.
    let link = &unsafe { &*ctx.cast::<Seam>() }.link;
    if frame.is_null() {
        link.count_rx_drop();
    } else {
        // SAFETY: the handler passes its copy of the frame, `len` bytes, for the call's duration.
        link.deliver(unsafe { core::slice::from_raw_parts(frame, len as usize) });
    }
}

unsafe extern "C" fn pull(ctx: *const c_void, buf: *mut u8, cap: c_uint) -> c_int {
    // SAFETY: as in `rx`.
    let seam = unsafe { &*ctx.cast::<Seam>() };
    let link = &seam.link;
    let Some(frame) = link.next_tx() else {
        seam.sleeper.going_to_sleep();
        return 0;
    };
    seam.sleeper.awake();
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
    let link = &unsafe { &*ctx.cast::<Seam>() }.link;
    link.transmitted(code == 0);
}

/// A `net_device`'s frames routed to a [`QueuePort`] until [`Attachment::detach`].
pub struct Attachment {
    seam: Rc<Seam>,
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
    let link = Rc::new(QueuePort::new(mac, mtu as usize, RX_QUEUE, TX_QUEUE));
    let seam = Rc::new(Seam { link, sleeper: Sleeper::default() });
    let mut netif = core::ptr::null_mut();
    // SAFETY: called on a task; the seam outlives the attachment, which `detach` ends.
    let code = unsafe { lx_netif_attach(raw, Rc::as_ptr(&seam).cast(), rx, pull, sent, &raw mut netif) };
    let Some(netif) = NonNull::new(netif).filter(|_| code == 0) else { return Err(code) };
    let group = MacAddr::ipv4_multicast(MDNS_GROUP.octets());
    // SAFETY: on a task, with the attachment live; the address is 6 readable bytes.
    let joined = unsafe { lx_netif_join(netif.as_ptr(), group.octets().as_ptr()) };
    if joined != 0 {
        log::warn!("net: joining {group} on ifindex {ifindex} failed ({joined}); mDNS queries will not arrive");
    }
    // SAFETY: on a task, with the attachment live.
    let tx_task = unsafe { lx_netif_tx_task(netif.as_ptr()) };
    // The scheduler wakes the thread: the C's `wake_up_process` would run Linux code on the main stack.
    // The kick is cleared in `detach` and on withdrawal, before the thread it wakes is stopped.
    let kicked = Rc::downgrade(&seam);
    seam.link.set_kick(Some(Box::new(move || {
        if let Some(seam) = kicked.upgrade()
            && seam.sleeper.wake_needed()
        {
            ferrite_lkpi::task::lx_task_unblock(tx_task);
        }
    })));
    log::info!(
        "net: frames of ifindex {ifindex} ({}, MTU {mtu}) go to the IP stack; at most {RX_QUEUE} queued each way, frames up to {FRAME_MAX} bytes",
        ferrite_nl80211::nl80211::MacAddr(mac)
    );
    Ok(Attachment { seam, netif, ifindex })
}

impl Attachment {
    pub fn link(&self) -> Rc<QueuePort> {
        Rc::clone(&self.seam.link)
    }

    /// Stop routing frames, from a task, and log the link's counters.
    pub fn detach(self) {
        self.seam.link.set_kick(None);
        // SAFETY: on a task; the attachment is live and ended once.
        unsafe { lx_netif_detach(self.netif.as_ptr()) };
        log::info!("net: ifindex {} detached; frames {}", self.ifindex, self.seam.link.stats());
    }
}
