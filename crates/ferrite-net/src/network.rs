//! Any number of stacks, one per port, polled together under one site.
//!
//! A port can be added at any time and removed when it goes away (a Wi-Fi link that drops); the
//! stacks run side by side, each with its own address, and the site's server, document and `Host`
//! check are shared by all of them.

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::time::Duration;

use ferrite_hal::{Clock, Instant, UnixTime};

use crate::http::{Site, Web};
use crate::notify::{Notification, Notifier, Stats};
use crate::port::Port;
use crate::sntp::{Fix, Sync};
use crate::stack::{PollCtx, Stack, StackConfig};
use crate::status::NetStatus;

/// A port in a [`Network`], valid until it is removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PortId(u32);

pub struct Network<S: Site> {
    web: Rc<Web<S>>,
    stacks: Vec<(PortId, Stack)>,
    next_id: u32,
    /// What the document showed at the last poll, to tell when it changed.
    key: Option<S::Key>,
    last: NetStatus,
    clock: Option<Box<dyn Clock>>,
    notifier: Option<Notifier>,
    sync: Option<Sync>,
}

impl<S: Site> Network<S> {
    pub fn new(site: S) -> Self {
        Self {
            web: Web::new(site),
            stacks: Vec::new(),
            next_id: 0,
            key: None,
            last: NetStatus::default(),
            clock: None,
            notifier: None,
            sync: None,
        }
    }

    /// Time the HTTP polls with `clock`, for the periodic summary in the log.
    #[must_use]
    pub fn timed(mut self, clock: impl Clock + 'static) -> Self {
        self.clock = Some(Box::new(clock));
        self
    }

    /// Send the notifications given to [`Network::notify`] with `notifier`, over the first port that
    /// has an address.
    pub fn set_notifier(&mut self, notifier: Notifier) {
        self.notifier = Some(notifier);
    }

    /// Keep the clock right with `sync`, asking over the first port that has an address; what it
    /// learns comes out of [`Network::take_time_fix`].
    pub fn set_sync(&mut self, sync: Sync) {
        self.sync = Some(sync);
    }

    /// The sync a finished round gave, once; `None` without a client or without news.
    pub fn take_time_fix(&mut self) -> Option<Fix> {
        self.sync.as_mut().and_then(Sync::take_fix)
    }

    /// The current time as the clock has it, which the time client places the server's timestamps by.
    pub fn set_time_pivot(&mut self, utc: UnixTime) {
        if let Some(sync) = &mut self.sync {
            sync.set_pivot(utc);
        }
    }

    /// Queue `notification` for delivery; false when there is no notifier or its config does not ask
    /// for the event.
    pub fn notify(&mut self, notification: Notification) -> bool {
        self.notifier.as_mut().is_some_and(|notifier| notifier.notify(notification))
    }

    /// Notifications queued or being sent.
    pub fn notifications_pending(&self) -> usize {
        self.notifier.as_ref().map_or(0, Notifier::pending)
    }

    pub fn notification_stats(&self) -> Option<Stats> {
        self.notifier.as_ref().map(Notifier::stats)
    }

    /// Start a stack on `port` at `now`.
    pub fn add(&mut self, port: Box<dyn Port>, config: StackConfig, now: Instant) -> PortId {
        let id = PortId(self.next_id);
        self.next_id += 1;
        let mut stack = Stack::new(port, config, now);
        if config.http {
            stack.serve(&self.web);
        }
        log::debug!("net: {} {} added, {}", stack.link().medium, stack.link().mac, stack.link());
        self.stacks.push((id, stack));
        id
    }

    /// Stop the stack of `id` and drop its port. False when there is none.
    pub fn remove(&mut self, id: PortId) -> bool {
        let Some(at) = self.stacks.iter().position(|(stack_id, _)| *stack_id == id) else {
            return false;
        };
        let (_, stack) = self.stacks.remove(at);
        if stack.notifying()
            && let Some(notifier) = &mut self.notifier
        {
            notifier.cancel();
        }
        if stack.syncing()
            && let Some(sync) = &mut self.sync
        {
            sync.cancel();
        }
        log::debug!("net: {} {} removed", stack.link().medium, stack.link().mac);
        true
    }

    pub fn stack(&self, id: PortId) -> Option<&Stack> {
        self.stacks.iter().find(|(stack_id, _)| *stack_id == id).map(|(_, stack)| stack)
    }

    pub fn stack_mut(&mut self, id: PortId) -> Option<&mut Stack> {
        self.stacks.iter_mut().find(|(stack_id, _)| *stack_id == id).map(|(_, stack)| stack)
    }

    pub fn ids(&self) -> impl Iterator<Item = PortId> + '_ {
        self.stacks.iter().map(|(id, _)| *id)
    }

    pub fn site(&self) -> &S {
        self.web.site()
    }

    pub fn is_empty(&self) -> bool {
        self.stacks.is_empty()
    }

    /// Every port's link, frames and services.
    pub fn status(&self) -> NetStatus {
        let mut status = NetStatus::default();
        self.stacks.iter().for_each(|(_, stack)| status.push(stack.status()));
        status
    }

    /// Advance every stack to `now`, with the app's `state` as the site sees it. Control requests
    /// the stacks received are passed to `commands`. Returns how long may pass before the next
    /// poll, or `None` when there is no port.
    pub fn poll(
        &mut self,
        now: Instant,
        state: &S::State<'_>,
        commands: &mut dyn FnMut(S::Command),
    ) -> Option<Duration> {
        if self.stacks.is_empty() {
            return None;
        }
        let status = self.status();
        let web = &*self.web;
        let key = web.site().key(state);
        if self.key.as_ref() != Some(&key) || status.links_differ(&self.last) {
            web.hub().changed();
            self.key = Some(key);
        }
        self.last = status;
        web.hub().set_addresses(status.ports().iter().filter_map(|port| port.link.address).map(|cidr| cidr.address()));

        let site = web.site();
        let mut render = |out: &mut alloc::string::String| site.render(state, &status, now, out);
        let leased = status.ports().iter().any(|port| port.link.address.is_some());
        if let Some(sync) = &mut self.sync {
            sync.observe(now, leased);
        }
        // The stack already sending carries on; else the first with an address takes the next one.
        let carrier = self.notifier.as_ref().filter(|notifier| notifier.pending() > 0).and_then(|_| {
            let sending = self.stacks.iter().find(|(_, stack)| stack.notifying());
            sending.or_else(|| self.stacks.iter().find(|(_, stack)| stack.link().address.is_some())).map(|(id, _)| *id)
        });
        let timekeeper = self.sync.as_ref().filter(|sync| sync.wants_port(now)).and_then(|_| {
            let asking = self.stacks.iter().find(|(_, stack)| stack.syncing());
            asking.or_else(|| self.stacks.iter().find(|(_, stack)| stack.link().address.is_some())).map(|(id, _)| *id)
        });
        let mut wait = Duration::MAX;
        for (id, stack) in &mut self.stacks {
            let notify = if Some(*id) == carrier { self.notifier.as_mut() } else { None };
            let sntp = if Some(*id) == timekeeper { self.sync.as_mut() } else { None };
            let mut ctx = PollCtx { render: &mut render, clock: self.clock.as_deref(), notify, sntp };
            wait = wait.min(stack.poll(now, &mut ctx));
        }
        web.drain_commands(commands);
        Some(wait)
    }
}
