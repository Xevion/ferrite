//! The `log` sink for every app: records go to the [`BootLog`] and, under the loader, to its link.

use alloc::string::String;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::fmt::{self, Write};
use core::time::Duration;

use ferrite_hal::Clock as _;
use log::{Level, LevelFilter, Log, Metadata, Record};

use crate::bootlog::BootLog;
use crate::clock::Clock;
use crate::link::{Link, Stream};

/// Records kept while the log is borrowed; further ones are dropped.
const MAX_DEFERRED: usize = 64;

struct Deferred {
    line: String,
    urgent: bool,
}

struct Sink {
    log: RefCell<Option<BootLog>>,
    clock: Cell<Option<Clock>>,
    deferred: RefCell<Vec<Deferred>>,
    link: Cell<Option<Link>>,
}

// SAFETY: UEFI boot services run single-threaded on the BSP, so the cells are never shared across threads.
unsafe impl Sync for Sink {}
// SAFETY: as above; there is no other thread to send it to.
#[expect(clippy::non_send_fields_in_send_ty, reason = "the log trait requires Send and nothing is ever sent")]
unsafe impl Send for Sink {}

static SINK: Sink =
    Sink { log: RefCell::new(None), clock: Cell::new(None), deferred: RefCell::new(Vec::new()), link: Cell::new(None) };

impl Sink {
    fn format(&self, level: Level, args: fmt::Arguments<'_>) -> String {
        let at = self.clock.get().map_or(Duration::ZERO, |clock| clock.now().since_epoch());
        let tag = match level {
            Level::Error => "ERROR ",
            Level::Warn => "WARN ",
            _ => "",
        };
        let mut line = String::new();
        // Formatting into a String only fails if a Display impl does, which then shows as a cut line.
        let _ = write!(line, "[{:>5}.{:03}] {tag}{args}", at.as_secs(), at.subsec_millis());
        line
    }

    /// Write straight to the log, or park the line if the log is already borrowed (a record logged
    /// from inside a save or a `with` closure), to be written on the next release. The link gets
    /// the record at once either way; a full link queue loses it from the link only.
    fn emit(&self, level: Level, args: fmt::Arguments<'_>) {
        let line = self.format(level, args);
        if let Some(link) = self.link.get() {
            let _ = link.write(Stream::Log, line.as_bytes());
        }
        let urgent = level == Level::Error;
        let Ok(mut slot) = self.log.try_borrow_mut() else {
            self.defer(Deferred { line, urgent });
            return;
        };
        if let Some(log) = slot.as_mut() {
            self.drain(log);
            Self::write(log, &line, urgent);
        }
    }

    fn defer(&self, record: Deferred) {
        if let Ok(mut queue) = self.deferred.try_borrow_mut()
            && queue.len() < MAX_DEFERRED
        {
            queue.push(record);
        }
    }

    fn drain(&self, log: &mut BootLog) {
        let parked = match self.deferred.try_borrow_mut() {
            Ok(mut queue) if !queue.is_empty() => core::mem::take(&mut *queue),
            _ => return,
        };
        for record in parked {
            Self::write(log, &record.line, record.urgent);
        }
    }

    fn write(log: &mut BootLog, line: &str, urgent: bool) {
        if urgent {
            log.milestone(line);
        } else {
            log.line(line);
        }
    }
}

impl Log for Sink {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &Record<'_>) {
        self.emit(record.level(), *record.args());
    }

    fn flush(&self) {
        let _ = with(BootLog::flush);
    }
}

/// Install the sink and adopt the log's clock for timestamps. Call once, right after `uefi::helpers::init`.
///
/// When a loader's link is installed, every record is also sent through it.
pub fn init(log: BootLog, level: LevelFilter) {
    SINK.clock.set(Some(log.clock()));
    attach_link();
    *SINK.log.borrow_mut() = Some(log);
    if log::set_logger(&SINK).is_ok() {
        log::set_max_level(level);
    }
}

/// Start sending records through the loader's link if one is installed now. The loader calls this
/// after installing its own link.
pub fn attach_link() {
    SINK.link.set(Link::locate());
}

/// Stop sending records through the link, before its protocol is uninstalled.
pub fn detach_link() {
    SINK.link.set(None);
}

/// Run `f` on the installed log, or return `None` before `init` or while it is borrowed.
/// Records logged during `f` are written once it returns.
pub fn with<R>(f: impl FnOnce(&mut BootLog) -> R) -> Option<R> {
    let mut slot = SINK.log.try_borrow_mut().ok()?;
    let log = slot.as_mut()?;
    let result = f(log);
    SINK.drain(log);
    Some(result)
}

/// Save a panic message to the log file now. Returns whether the console already showed it.
#[cfg(all(any(target_os = "uefi", target_os = "none"), feature = "panic-handler"))]
pub(crate) fn record_panic(info: &core::panic::PanicInfo<'_>) -> bool {
    use crate::bootlog::Echo;

    let echoed = with(|log| log.echo() == Echo::Console);
    SINK.emit(Level::Error, format_args!("panic: {info}"));
    let _ = with(BootLog::flush);
    echoed.unwrap_or(false)
}
