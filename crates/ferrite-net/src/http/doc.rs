//! The site's document as serialized bytes, shared by every request and event stream on every stack.
//!
//! It is rendered only when a reader finds none that is current: one rendered since what it shows
//! last changed and less than [`MAX_AGE`] ago. A render writes into the previous document's buffer
//! unless a response is still sending that one, so a steady stream of requests makes no allocation
//! and no more than four renders a second.

use alloc::rc::Rc;
use alloc::string::String;
use core::time::Duration;

use ferrite_hal::Instant;
use picoserve::io::Write;
use picoserve::response::Content;

/// How long a rendered document answers readers. Clock-derived fields and traffic counters in it
/// are at most this stale.
pub const MAX_AGE: Duration = Duration::from_millis(250);

/// Sized so a render of a typical document does not grow the buffer.
const INITIAL_CAPACITY: usize = 32 << 10;

/// A rendered document, as a response body or an event's data.
#[derive(Clone, Debug)]
pub struct Document {
    text: Rc<String>,
    generation: u64,
    content_type: &'static str,
}

impl Document {
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The [`Doc::generation`] it was rendered at.
    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

impl Content for Document {
    fn content_type(&self) -> &'static str {
        self.content_type
    }

    fn content_length(&self) -> usize {
        self.text.len()
    }

    async fn write_content<W: Write>(self, mut writer: W) -> Result<(), W::Error> {
        writer.write_all(self.text.as_bytes()).await
    }
}

pub struct Doc {
    content_type: &'static str,
    /// Counts changes to what the document shows apart from the clock and the counters.
    generation: u64,
    rendered: Option<Rendered>,
    text: Rc<String>,
}

struct Rendered {
    generation: u64,
    at: Instant,
}

impl Doc {
    pub fn new(content_type: &'static str) -> Self {
        Self { content_type, generation: 0, rendered: None, text: Rc::new(String::with_capacity(INITIAL_CAPACITY)) }
    }

    /// What the document shows has changed.
    pub const fn bump(&mut self) {
        self.generation += 1;
    }

    /// Changes whenever the document, the clock aside, would read differently.
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// The document if one rendered at the current generation is still younger than [`MAX_AGE`].
    pub fn get(&self, now: Instant) -> Option<Document> {
        let rendered = self.rendered.as_ref()?;
        (rendered.generation == self.generation && now - rendered.at < MAX_AGE).then(|| Document {
            text: Rc::clone(&self.text),
            generation: rendered.generation,
            content_type: self.content_type,
        })
    }

    /// Serialize the document as of `now` with `write`.
    pub fn render(&mut self, now: Instant, write: &mut dyn FnMut(&mut String)) {
        if Rc::strong_count(&self.text) > 1 {
            // A response is still sending the previous document; leave it intact.
            self.text = Rc::new(String::with_capacity(self.text.capacity()));
        }
        let text = Rc::make_mut(&mut self.text);
        text.clear();
        write(text);
        self.rendered = Some(Rendered { generation: self.generation, at: now });
    }
}

#[cfg(test)]
mod tests {
    use alloc::borrow::ToOwned as _;
    use core::fmt::Write as _;

    use assert2::check;

    use super::*;

    fn at(ms: u64) -> Instant {
        Instant::from_epoch(Duration::from_millis(ms))
    }

    /// Render if no current document exists, as one poll would, and return the one that answers.
    fn poll(doc: &mut Doc, ms: u64) -> Document {
        if doc.get(at(ms)).is_none() {
            doc.render(at(ms), &mut |out| {
                let _ = write!(out, "at {ms}");
            });
        }
        doc.get(at(ms)).unwrap()
    }

    #[test]
    fn readers_close_together_share_one_render() {
        let mut doc = Doc::new("text/plain");
        let first = poll(&mut doc, 1_000);
        let second = poll(&mut doc, 1_100);
        check!(Rc::ptr_eq(&first.text, &second.text));
        check!(second.as_str() == "at 1000");
    }

    #[test]
    fn a_change_or_the_age_limit_renders_again() {
        let mut doc = Doc::new("text/plain");
        let first = poll(&mut doc, 1_000);
        doc.bump();
        let second = poll(&mut doc, 1_010);
        check!(second.generation() != first.generation());
        check!(second.as_str() == "at 1010");
        let third = poll(&mut doc, 1_300);
        check!(third.generation() == second.generation());
        check!(third.as_str() == "at 1300");
    }

    #[test]
    fn a_render_leaves_a_document_still_being_sent_alone() {
        let mut doc = Doc::new("text/plain");
        let sending = poll(&mut doc, 1_000);
        let before = sending.as_str().to_owned();
        doc.bump();
        let next = poll(&mut doc, 1_010);
        check!(sending.as_str() == before);
        check!(next.as_str() != before);
    }

    #[test]
    fn a_render_reuses_the_buffer_when_nothing_holds_the_old_document() {
        let mut doc = Doc::new("text/plain");
        let ptr = poll(&mut doc, 1_000).as_str().as_ptr();
        doc.bump();
        check!(poll(&mut doc, 1_300).as_str().as_ptr() == ptr);
    }
}
