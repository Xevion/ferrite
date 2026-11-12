//! The main loop: poll the network every few milliseconds, render at about
//! 20 Hz, and give the time in between to the memory sweep.

use alloc::format;
use core::time::Duration;

use ferrite_efi::clock::Clock;
use ratatui::Terminal;
use uefi::Event;
use uefi::boot::{self, EventType, TimerTrigger, Tpl};
use uefi::proto::console::text::{Input, Key, Output, ScanCode};

use crate::backend::{BackendError, UefiBackend};
use crate::diag::{self, Diag};
use crate::net::Net;
use crate::sweep::Sweep;
use crate::ui::{self, View};

const SWEEP_BYTES: usize = 64 << 20;
const FRAME_INTERVAL: Duration = Duration::from_millis(50);
/// Sweep chunks are sized to take about this long, so the network is polled in time.
const SWEEP_SLICE: Duration = Duration::from_millis(1);
const MIN_CHUNK_BYTES: usize = 64 << 10;
const MAX_CHUNK_BYTES: usize = 4 << 20;
const START_CHUNK_BYTES: usize = 256 << 10;

/// Sleeps until a timeout or a keypress, for when there is no sweep to run.
struct Waiter {
    /// The timer, then the console's key event.
    events: [Event; 2],
}

impl Waiter {
    fn new(input: &Input) -> uefi::Result<Self> {
        // SAFETY: a plain timer event with no notify function.
        let timer = unsafe { boot::create_event(EventType::TIMER, Tpl::APPLICATION, None, None) }?;
        let key = input.wait_for_key_event()?;
        Ok(Self { events: [timer, key] })
    }

    fn wait(&mut self, timeout: Duration) -> uefi::Result<()> {
        boot::set_timer(&self.events[0], TimerTrigger::Relative(timeout))?;
        boot::wait_for_event(&mut self.events).map_err(|e| e.to_err_without_payload())?;
        Ok(())
    }
}

impl Drop for Waiter {
    fn drop(&mut self) {
        // SAFETY: the clone is the only handle used from here on; the original is dropped unused.
        let timer = unsafe { self.events[0].unsafe_clone() };
        if let Err(e) = boot::close_event(timer) {
            log::debug!("ui: closing the timer event failed: {e}");
        }
    }
}

pub struct App<'a> {
    input: &'a mut Input,
    terminal: Terminal<UefiBackend<'a>>,
    clock: Clock,
    net: Option<Net>,
    net_err: Option<&'static str>,
    sweep: Sweep,
    chunk_bytes: usize,
    diag: Diag,
    waiter: Option<Waiter>,
    last_view: Option<View>,
    draw_failures: u32,
}

impl<'a> App<'a> {
    pub fn new(input: &'a mut Input, output: &'a mut Output) -> Result<Self, BackendError> {
        let terminal = Terminal::new(UefiBackend::new(output))?;
        let clock = Clock::start();
        let (net, net_err) = match Net::setup(Duration::from_millis(clock.ms())) {
            Ok(net) => {
                log::debug!("net: SNP up, {}", net.status());
                (Some(net), None)
            },
            Err(e) => {
                diag::milestone(&format!("net: setup failed: {e}"));
                (None, Some(e.reason()))
            },
        };
        let waiter = Waiter::new(input).inspect_err(|e| log::warn!("ui: no wait events, stalling instead: {e}")).ok();
        Ok(Self {
            input,
            terminal,
            clock,
            net,
            net_err,
            sweep: Sweep::new(SWEEP_BYTES),
            chunk_bytes: START_CHUNK_BYTES,
            diag: Diag::new(),
            waiter,
            last_view: None,
            draw_failures: 0,
        })
    }

    fn now(&self) -> Duration {
        Duration::from_millis(self.clock.ms())
    }

    /// Run until `q` or `Esc`.
    pub fn run(&mut self) {
        let mut next_frame = Duration::ZERO;
        loop {
            let now = self.now();
            let sweep = self.sweep.status();
            let net_wait = self.net.as_mut().map_or(FRAME_INTERVAL, |net| net.poll(now, &sweep));
            if self.quit_requested() {
                return;
            }
            let net = self.net.as_ref().map(Net::status);
            if now >= next_frame {
                self.draw(View { sweep, net, net_err: self.net_err });
                next_frame = now.saturating_add(FRAME_INTERVAL);
            }
            self.diag.tick(now, net.as_ref(), &sweep);
            self.idle_until(now.saturating_add(net_wait).min(next_frame));
        }
    }

    fn quit_requested(&mut self) -> bool {
        let mut quit = false;
        while let Ok(Some(key)) = self.input.read_key() {
            quit |= match key {
                Key::Printable(c) => matches!(char::from(c), 'q' | 'Q'),
                Key::Special(code) => code == ScanCode::ESCAPE,
            };
        }
        quit
    }

    /// Sweep until `deadline` (at least one chunk), or wait on the timer and key
    /// events when there is nothing to sweep. Without events the loop just spins.
    fn idle_until(&mut self, deadline: Duration) {
        if !self.sweep.has_work() {
            let timeout = deadline.saturating_sub(self.now());
            if let Some(waiter) = self.waiter.as_mut()
                && let Err(e) = waiter.wait(timeout)
            {
                log::warn!("ui: waiting on events failed, polling instead: {e}");
                self.waiter = None;
            }
            return;
        }
        loop {
            let start = self.now();
            self.sweep.step(self.chunk_bytes);
            let end = self.now();
            let took = end.saturating_sub(start);
            if took < SWEEP_SLICE {
                self.chunk_bytes = (self.chunk_bytes * 2).min(MAX_CHUNK_BYTES);
            } else if took > SWEEP_SLICE * 2 {
                self.chunk_bytes = (self.chunk_bytes / 2).max(MIN_CHUNK_BYTES);
            }
            if end >= deadline {
                return;
            }
        }
    }

    fn draw(&mut self, view: View) {
        if self.last_view == Some(view) {
            return;
        }
        match self.terminal.draw(|frame| ui::render(frame, &view)) {
            Ok(_) => self.last_view = Some(view),
            Err(e) => {
                if self.draw_failures == 0 {
                    log::warn!("ui: drawing failed: {e}");
                }
                self.draw_failures = self.draw_failures.saturating_add(1);
                self.last_view = None;
            },
        }
    }
}
