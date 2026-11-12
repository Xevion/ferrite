//! The `log` sink for every app: records go to the [`BootLog`].

use core::cell::RefCell;

use log::{LevelFilter, Log, Metadata, Record};

use crate::bootlog::BootLog;

struct Sink(RefCell<Option<BootLog>>);

// SAFETY: UEFI boot services run single-threaded on the BSP, so the cell is never shared across threads.
unsafe impl Sync for Sink {}
// SAFETY: as above; there is no other thread to send it to.
unsafe impl Send for Sink {}

static SINK: Sink = Sink(RefCell::new(None));

impl Log for Sink {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &Record<'_>) {
        // A record logged while the log is borrowed (from inside a flush) is dropped rather than deadlocking.
        if let Ok(mut slot) = self.0.try_borrow_mut()
            && let Some(log) = slot.as_mut()
        {
            log.line(&alloc::format!("{}", record.args()));
        }
    }

    fn flush(&self) {
        let _ = with(BootLog::flush);
    }
}

/// Install the sink. Call once, right after `uefi::helpers::init`.
pub fn init(log: BootLog, level: LevelFilter) {
    *SINK.0.borrow_mut() = Some(log);
    if log::set_logger(&SINK).is_ok() {
        log::set_max_level(level);
    }
}

/// Run `f` on the installed log, or return `None` before `init` or while it is borrowed.
pub fn with<R>(f: impl FnOnce(&mut BootLog) -> R) -> Option<R> {
    SINK.0.try_borrow_mut().ok()?.as_mut().map(f)
}
