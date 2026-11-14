//! The status document as serialized bytes, shared by every request and event stream.
//!
//! It is rendered only when a reader finds none that is current: one rendered since the run or
//! the link last changed and less than [`MAX_AGE`] ago. A render writes into the previous
//! document's buffer unless a response is still sending that one, so a steady stream of requests
//! makes no allocation and no more than four renders a second.

use alloc::rc::Rc;
use alloc::string::String;
use core::time::Duration;

use ferrite_hal::Instant;
use picoserve::io::Write;
use picoserve::response::Content;
use smoltcp::wire::Ipv4Address;

use super::json::{StatusDoc, ToJson as _};
use crate::net::{Link, NetStatus};
use crate::report::Snapshot;
use crate::sweep::SweepStatus;

/// How long a rendered document answers readers. The clock-derived fields (uptime and elapsed
/// times) and the traffic counters are at most this stale.
pub const MAX_AGE: Duration = Duration::from_millis(250);

/// Sized for a full history and fault log, so a render does not grow the buffer.
const INITIAL_CAPACITY: usize = 32 << 10;

/// What the document shows besides the clock and the traffic counters. The counters change with
/// every frame, including the acknowledgements of the documents sent, so they never trigger a
/// render or an event by themselves.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Key {
    status: SweepStatus,
    /// Moves whenever the finished tests, passes or processors do.
    revision: u64,
    /// The fault log's length, since the status counts faults but not the log's contents.
    faults: usize,
    link: Link,
}

/// A rendered document, as a response body or an event's data.
#[derive(Clone, Debug)]
pub struct Document {
    text: Rc<String>,
    generation: u64,
}

impl Document {
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The [`Cache::generation`] it was rendered at.
    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

impl Content for Document {
    fn content_type(&self) -> &'static str {
        "application/json"
    }

    fn content_length(&self) -> usize {
        self.text.len()
    }

    async fn write_content<W: Write>(self, mut writer: W) -> Result<(), W::Error> {
        writer.write_all(self.text.as_bytes()).await
    }
}

pub struct Cache {
    key: Option<Key>,
    net: NetStatus,
    /// Counts changes to what the document shows apart from the clock and the counters.
    generation: u64,
    rendered: Option<Rendered>,
    text: Rc<String>,
}

struct Rendered {
    generation: u64,
    at: Instant,
}

impl Cache {
    pub fn new() -> Self {
        Self {
            key: None,
            net: NetStatus::default(),
            generation: 0,
            rendered: None,
            text: Rc::new(String::with_capacity(INITIAL_CAPACITY)),
        }
    }

    /// Note what the sweep and the network look like now.
    pub fn update(&mut self, snapshot: &Snapshot<'_>, net: NetStatus) {
        let key = Key {
            status: snapshot.status,
            revision: snapshot.history.revision,
            faults: snapshot.faults.len(),
            link: net.link,
        };
        if self.key != Some(key) {
            self.key = Some(key);
            self.generation += 1;
        }
        self.net = net;
    }

    /// Changes whenever the document, the clock aside, would read differently.
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Our current address, for checking what a request was addressed to.
    pub fn address(&self) -> Option<Ipv4Address> {
        self.net.link.address.map(|cidr| cidr.address())
    }

    /// The document if one rendered at the current generation is still younger than [`MAX_AGE`].
    pub fn get(&self, now: Instant) -> Option<Document> {
        let rendered = self.rendered.as_ref()?;
        (rendered.generation == self.generation && now - rendered.at < MAX_AGE)
            .then(|| Document { text: Rc::clone(&self.text), generation: rendered.generation })
    }

    /// Serialize `snapshot` as of `now`, which must be what [`Cache::update`] last saw.
    pub fn render(&mut self, snapshot: &Snapshot<'_>, now: Instant) {
        if Rc::strong_count(&self.text) > 1 {
            // A response is still sending the previous document; leave it intact.
            self.text = Rc::new(String::with_capacity(self.text.capacity()));
        }
        let text = Rc::make_mut(&mut self.text);
        text.clear();
        StatusDoc { snapshot, net: &self.net, now }.to_json(text);
        self.rendered = Some(Rendered { generation: self.generation, at: now });
    }
}

#[cfg(test)]
mod tests {
    use alloc::borrow::ToOwned as _;
    use alloc::collections::VecDeque;

    use assert2::check;

    use super::*;
    use crate::net::http::testing::idle_status;
    use crate::report::History;

    fn at(ms: u64) -> Instant {
        Instant::from_epoch(Duration::from_millis(ms))
    }

    struct Run {
        cache: Cache,
        history: History,
        faults: VecDeque<crate::report::FaultRecord>,
        net: NetStatus,
    }

    impl Run {
        fn new() -> Self {
            Self {
                cache: Cache::new(),
                history: History::default(),
                faults: VecDeque::new(),
                net: NetStatus::default(),
            }
        }

        /// Update and then render if no current document exists, as one network poll would.
        fn poll(&mut self, ms: u64) -> Document {
            let snapshot = Snapshot { status: idle_status(), history: &self.history, faults: &self.faults };
            self.cache.update(&snapshot, self.net);
            if self.cache.get(at(ms)).is_none() {
                self.cache.render(&snapshot, at(ms));
            }
            self.cache.get(at(ms)).unwrap()
        }
    }

    #[test]
    fn unchanged_inputs_are_served_from_one_render() {
        let mut run = Run::new();
        let first = run.poll(1_000);
        let second = run.poll(1_100);
        check!(Rc::ptr_eq(&first.text, &second.text));
        check!(second.as_str().contains("\"uptime_ms\":1000,"), "{}", second.as_str());
    }

    #[test]
    fn a_new_revision_or_fault_count_or_link_renders_again() {
        let mut run = Run::new();
        let first = run.poll(1_000);

        run.history.revision += 1;
        let second = run.poll(1_010);
        check!(second.generation() != first.generation());
        check!(second.as_str().contains("\"uptime_ms\":1010,"));

        run.net.link.up = true;
        let third = run.poll(1_020);
        check!(third.generation() != second.generation());
        check!(third.as_str().contains("\"link_up\":true"));
    }

    #[test]
    fn traffic_counters_wait_for_the_age_limit() {
        let mut run = Run::new();
        let first = run.poll(1_000);

        run.net.counters.rx_frames = 9;
        let second = run.poll(1_100);
        check!(second.generation() == first.generation());
        check!(second.as_str().contains("\"rx_frames\":0"));

        let third = run.poll(1_300);
        check!(third.generation() == first.generation());
        check!(third.as_str().contains("\"rx_frames\":9"));
        check!(third.as_str().contains("\"uptime_ms\":1300,"));
    }

    #[test]
    fn a_render_leaves_a_document_still_being_sent_alone() {
        let mut run = Run::new();
        let sending = run.poll(1_000);
        let before = sending.as_str().to_owned();

        run.history.revision += 1;
        let next = run.poll(1_010);
        check!(sending.as_str() == before);
        check!(next.as_str() != before);
    }

    #[test]
    fn a_render_reuses_the_buffer_when_nothing_holds_the_old_document() {
        let mut run = Run::new();
        let ptr = run.poll(1_000).as_str().as_ptr();
        run.history.revision += 1;
        check!(run.poll(1_300).as_str().as_ptr() == ptr);
    }
}
