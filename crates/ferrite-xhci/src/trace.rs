//! What the receive path saw lately and where the rings stand, for a link that delivers late or not
//! at all. Kept small and fixed so it costs nothing while the link is healthy.

use core::fmt;

use ferrite_hal::Instant;

use crate::stats::RegSnapshot;

/// Receive events kept, newest last.
pub const IN_TRACE_LEN: usize = 16;

/// One transfer event on the IN (host to device) endpoint, or one that matched no TRB at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InEvent {
    pub at: Instant,
    /// The endpoint the event names.
    pub endpoint: u8,
    /// Slot in the IN ring the event names, when it names one.
    pub index: Option<u16>,
    pub completion: u8,
    pub residual: u32,
    /// TRBs the event retired; 0 when it matched nothing in flight.
    pub retired: u16,
    /// Bytes handed to the receiver.
    pub delivered: u32,
}

/// The last [`IN_TRACE_LEN`] receive events.
#[derive(Clone, Debug, Default)]
pub struct InTrace {
    events: [Option<InEvent>; IN_TRACE_LEN],
    next: usize,
}

impl InTrace {
    pub const fn record(&mut self, event: InEvent) {
        self.events[self.next] = Some(event);
        self.next = (self.next + 1) % IN_TRACE_LEN;
    }

    /// Oldest first.
    pub fn iter(&self) -> impl Iterator<Item = &InEvent> {
        let (newer, older) = self.events.split_at(self.next);
        older.iter().chain(newer).flatten()
    }
}

/// Where a producer ring stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RingPos {
    pub enqueue: usize,
    pub dequeue: usize,
    pub pending: usize,
    pub cycle: bool,
}

impl fmt::Display for RingPos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "enq {} deq {} pending {} cycle {}", self.enqueue, self.dequeue, self.pending, u8::from(self.cycle))
    }
}

/// A look at the whole receive and event path at one moment.
#[derive(Clone, Debug)]
pub struct Diagnostics {
    pub at: Instant,
    pub regs: RegSnapshot,
    pub in_ring: RingPos,
    pub out_ring: RingPos,
    pub event_dequeue: usize,
    pub event_cycle: bool,
    pub in_done: u32,
    pub in_empty: u32,
    pub unknown_events: u32,
    pub bytes_received: u64,
    pub trace: InTrace,
}

impl fmt::Display for Diagnostics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ms = |t: Instant| t.since_epoch().as_millis();
        writeln!(
            f,
            "dbc at t={}ms: in ring {}; out ring {}; event deq {} cycle {}; in done {} empty {} unknown events {}, {} bytes received",
            ms(self.at),
            self.in_ring,
            self.out_ring,
            self.event_dequeue,
            u8::from(self.event_cycle),
            self.in_done,
            self.in_empty,
            self.unknown_events,
            self.bytes_received,
        )?;
        writeln!(f, "{}", self.regs)?;
        write!(f, "last receive events:")?;
        for e in self.trace.iter() {
            write!(
                f,
                "\n  t={}ms ep {} trb {} cc {} residual {} retired {} delivered {}",
                ms(e.at),
                e.endpoint,
                e.index.map_or(-1, i32::from),
                e.completion,
                e.residual,
                e.retired,
                e.delivered,
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;
    use rstest::rstest;

    use super::*;

    fn event(n: u32) -> InEvent {
        InEvent {
            at: Instant::from_epoch(core::time::Duration::from_millis(u64::from(n))),
            endpoint: 3,
            index: Some(0),
            completion: 1,
            residual: 0,
            retired: 1,
            delivered: n,
        }
    }

    #[rstest]
    #[case::empty(0, &[])]
    #[case::partial(3, &[0, 1, 2])]
    #[case::exactly_full(IN_TRACE_LEN as u32, &(0..IN_TRACE_LEN as u32).collect::<std::vec::Vec<_>>())]
    #[case::wrapped(IN_TRACE_LEN as u32 + 5, &(5..IN_TRACE_LEN as u32 + 5).collect::<std::vec::Vec<_>>())]
    fn the_trace_keeps_the_newest_events_oldest_first(#[case] recorded: u32, #[case] want: &[u32]) {
        let mut trace = InTrace::default();
        (0..recorded).for_each(|n| trace.record(event(n)));
        let got: std::vec::Vec<u32> = trace.iter().map(|e| e.delivered).collect();
        check!(got == want);
    }
}
