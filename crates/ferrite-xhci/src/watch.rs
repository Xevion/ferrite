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

    /// Log the registers if the link state changed since the last call. Returns whether it did.
    ///
    /// DCST's event-ring-not-empty bit is left out of the comparison: it flips on
    /// every transfer and says nothing about the link.
    pub fn observe(&mut self, now: Instant, ctrl: DcCtrl, status: DcSt, portsc: PortSc) -> bool {
        let seen = (ctrl, status.without_event_pending(), portsc);
        if self.last == Some(seen) {
            return false;
        }
        self.last = Some(seen);
        if self.budget == 0 {
            return false;
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
        true
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    #[test]
    fn event_ring_bit_alone_is_not_a_change() {
        let mut watch = LinkWatch::new();
        let ctrl = DcCtrl::new_with_raw_value(0x8a00_0003);
        let portsc = PortSc::new_with_raw_value(0x0000_1203);
        let t = Instant::default();
        check!(watch.observe(t, ctrl, DcSt::new_with_raw_value(0x0300_0000), portsc));
        check!(!watch.observe(t, ctrl, DcSt::new_with_raw_value(0x0300_0001), portsc));
        check!(!watch.observe(t, ctrl, DcSt::new_with_raw_value(0x0300_0000), portsc));
        check!(watch.observe(t, ctrl, DcSt::new_with_raw_value(0x0000_0000), portsc), "debug port lost");
    }
}
