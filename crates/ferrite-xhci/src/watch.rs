//! Logs each change of the DbC link registers, so a report shows the link timeline.

use ferrite_hal::Instant;

use crate::regs::{DcCtrl, DcSt, PortSc};

/// Register changes logged before the watcher goes quiet; a flapping link would
/// otherwise flood the log.
const WATCH_BUDGET: u32 = 200;

pub struct LinkWatch {
    last: Option<(DcCtrl, DcSt, PortSc)>,
    budget: u32,
}

impl LinkWatch {
    pub const fn new() -> Self {
        Self { last: None, budget: WATCH_BUDGET }
    }

    /// Log the next observation even if it matches the last one.
    pub fn forget(&mut self) {
        self.last = None;
    }

    pub fn observe(&mut self, now: Instant, ctrl: DcCtrl, status: DcSt, portsc: PortSc) {
        let seen = (ctrl, status, portsc);
        if self.last == Some(seen) {
            return;
        }
        self.last = Some(seen);
        if self.budget == 0 {
            return;
        }
        self.budget -= 1;
        log::debug!(
            "t={}ms DCCTRL={:#010x} DCST={:#010x} DCPORTSC={:#010x} ({} {} {})",
            now.since_epoch().as_millis(),
            ctrl.raw_value(),
            status.raw_value(),
            portsc.raw_value(),
            if portsc.ccs() { "connected" } else { "disconnected" },
            portsc.link(),
            portsc.speed_code(),
        );
    }
}
