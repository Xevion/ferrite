//! The loader's life while the link is up: receive pushes, run apps, answer the desktop.

use alloc::format;
use alloc::string::ToString;
use core::fmt::Display;
use core::time::Duration;

use ferrite_efi::bootlog::BootLog;
use ferrite_efi::clock::Clock;
use ferrite_efi::link::hub::{Mode, Transport};
use ferrite_efi::link::provider::Provider;
use ferrite_efi::link::receive::{Image, Receiver};
use ferrite_efi::logger;
use ferrite_hal::{Clock as _, Instant};
use ferrite_wire::link::{Exited, Message, PushBegin, PushKind, ShortStr};
use uefi::{Handle, Status, boot};

use crate::image;

/// How often the link state is looked at for the status line.
const STATUS_EVERY: Duration = Duration::from_millis(250);
/// How long a replacement loader's `Started` gets to leave before the link is torn down.
const HANDOVER_FLUSH: Duration = Duration::from_millis(500);

/// A replacement loader, loaded and ready to start once the link is released.
pub struct Chain {
    pub child: Handle,
    pub name: ShortStr,
    pub watchdog_secs: u32,
}

struct LastRun {
    name: ShortStr,
    status: Status,
    elapsed: Duration,
}

struct Server<'a, T: Transport> {
    provider: &'a Provider<T>,
    clock: Clock,
    push: Option<Receiver>,
    last: Option<LastRun>,
    shown: Option<&'static str>,
    next_status: Instant,
}

/// Serve the desktop until it pushes a replacement loader.
pub fn serve<T: Transport>(provider: &Provider<T>, clock: Clock) -> Chain {
    let mut server = Server { provider, clock, push: None, last: None, shown: None, next_status: clock.now() };
    loop {
        provider.pump();
        while let Some(command) = provider.next_command() {
            if let Some(chain) = server.handle(command) {
                return chain;
            }
        }
        server.idle();
    }
}

impl<T: Transport> Server<'_, T> {
    fn handle(&mut self, command: Message) -> Option<Chain> {
        match command {
            Message::PushBegin(begin) => self.begin(begin),
            Message::PushData { offset, bytes } => self.data(offset, &bytes),
            Message::PushEnd => return self.end(),
            Message::Abort => self.abort(),
            _ => {},
        }
        None
    }

    fn send(&self, msg: &Message) {
        if let Err(e) = self.provider.send(msg) {
            log::warn!("{:?} not sent: {e}", msg.message_type());
        }
    }

    fn reject(&mut self, reason: impl Display) {
        log::warn!("push rejected: {reason}");
        self.push = None;
        self.send(&Message::PushRejected { reason: reason.to_string() });
    }

    fn fault(&self, text: impl Display) {
        log::error!("{text}");
        self.send(&Message::Fault { text: text.to_string() });
    }

    fn begin(&mut self, begin: PushBegin) {
        if self.push.take().is_some() {
            log::info!("a push in progress was replaced");
        }
        let (name, size, kind, watchdog) = (begin.name.clone(), begin.size, begin.kind, begin.watchdog_secs);
        match Receiver::start(begin) {
            Ok(receiver) => {
                log::info!("push of {name}: {size} bytes, {kind:?}, watchdog {watchdog} s");
                self.push = Some(receiver);
                self.send(&Message::PushAccepted);
            },
            Err(reject) => self.reject(reject),
        }
    }

    /// Chunks with no push in progress follow a rejection or an abort and are ignored.
    fn data(&mut self, offset: u64, bytes: &[u8]) {
        let Some(receiver) = self.push.as_mut() else {
            return;
        };
        if let Err(reject) = receiver.accept(offset, bytes) {
            self.reject(reject);
        }
    }

    fn abort(&mut self) {
        if let Some(receiver) = self.push.take() {
            log::info!(
                "push of {} aborted at {} of {} bytes",
                receiver.begin().name,
                receiver.received(),
                receiver.begin().size
            );
        }
    }

    fn end(&mut self) -> Option<Chain> {
        let receiver = self.push.take()?;
        let image = match receiver.finish() {
            Ok(image) => image,
            Err(reject) => {
                self.reject(reject);
                return None;
            },
        };
        match image.begin.kind {
            PushKind::App => {
                self.run_app(image);
                None
            },
            PushKind::Loader => self.prepare_chain(image),
            PushKind::Unknown(_) => None,
        }
    }

    fn run_app(&mut self, image: Image) {
        let Image { begin, bytes } = image;
        let child = match image::load(&bytes) {
            Ok(child) => child,
            Err(e) => return self.reject(format!("the firmware refused the image: {e}")),
        };
        drop(bytes);
        if let Err(e) = image::arm(begin.watchdog_secs) {
            image::unload(child);
            return self.fault(format!("{} not started: the watchdog did not arm: {e}", begin.name));
        }

        log::info!("starting {} with a {} s watchdog", begin.name, begin.watchdog_secs);
        self.provider.set_mode(Mode::Running);
        self.send(&Message::Started { name: begin.name.clone() });
        let started = self.clock.now();
        let result = boot::start_image(child);
        let elapsed = self.clock.elapsed(started);
        self.provider.set_mode(Mode::Idle);
        image::disarm();

        let status = result.err().map_or(Status::SUCCESS, |e| e.status());
        // These two mean the image never ran, so the firmware has not unloaded it.
        if matches!(status, Status::INVALID_PARAMETER | Status::SECURITY_VIOLATION) {
            image::unload(child);
        }
        self.send(&Message::Exited(Exited::new(image::code(status), elapsed)));
        log::info!("{} exited with {status} after {} ms", begin.name, elapsed.as_millis());
        self.last = Some(LastRun { name: begin.name, status, elapsed });
        self.show_status();
    }

    fn prepare_chain(&mut self, image: Image) -> Option<Chain> {
        let Image { begin, bytes } = image;
        let child = match image::load(&bytes) {
            Ok(child) => child,
            Err(e) => {
                self.reject(format!("the firmware refused the loader image: {e}"));
                return None;
            },
        };
        drop(bytes);
        log::info!("replacing the loader with {}", begin.name);
        self.send(&Message::Started { name: begin.name.clone() });
        let begun = self.clock.now();
        while self.provider.pending() > 0 && self.clock.elapsed(begun) < HANDOVER_FLUSH {
            self.provider.pump();
        }
        Some(Chain { child, name: begin.name, watchdog_secs: begin.watchdog_secs })
    }

    /// Between messages: save the log now and then, and report a link state change.
    fn idle(&mut self) {
        logger::with(BootLog::poll);
        let now = self.clock.now();
        if now < self.next_status {
            return;
        }
        self.next_status = now + STATUS_EVERY;
        let state = self.provider.link_status();
        if state != self.shown {
            self.shown = state;
            self.show_status();
        }
    }

    fn show_status(&self) {
        let link = self.provider.link_status().unwrap_or("busy");
        let dropped = self.provider.stats().map_or(0, |s| s.rx_dropped);
        match &self.last {
            Some(last) => log::info!(
                "status: link {link}; last app {} exited {} after {} ms; {dropped} rx bytes dropped",
                last.name,
                last.status,
                last.elapsed.as_millis()
            ),
            None => log::info!("status: link {link}; no app run yet; {dropped} rx bytes dropped"),
        }
    }
}
