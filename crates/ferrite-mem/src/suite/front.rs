//! Where a running test is working, for a map of the memory it covers.

use super::plan::Span;
use crate::ops::Order;

/// What a test is doing to its memory at one moment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activity {
    /// Walking the span once, in `order`, with `done` words walked so far.
    Sweep { order: Order, done: usize },
    /// The span holds its pattern untouched until a wait ends.
    Hold,
    /// Block moves inside the span, a window that is worked whole.
    Window,
    /// Bus stress: the span is the stream being filled, ascending with `done` words filled, while
    /// `checked` words of the stream `check` have been verified, also ascending.
    Bus { done: usize, check: Span, checked: usize },
}

/// The phase a test is on, in words of the whole layout: the span it works inside the test's
/// share, and how far along it is. Cheap to take, so a harness can publish one every chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Front {
    /// The phase's words inside the share; for a bus phase, those of its fill stream.
    pub span: Span,
    pub activity: Activity,
}

impl Front {
    /// The part of the span the phase has finished with, behind the head. For a bus phase, of the
    /// fill stream; nothing for a hold or a window, which finish as a whole.
    #[must_use]
    pub fn done(&self) -> Span {
        let (order, done) = match self.activity {
            Activity::Sweep { order, done } => (order, done),
            Activity::Bus { done, .. } => (Order::Ascending, done),
            Activity::Hold | Activity::Window => return Span { start: self.span.start, words: 0 },
        };
        let words = done.min(self.span.words);
        match order {
            Order::Ascending => Span { start: self.span.start, words },
            Order::Descending => Span { start: self.span.end() - words, words },
        }
    }

    /// The next word the phase touches (its first block, for a window); `None` for a hold, or
    /// when the span is empty or finished.
    #[must_use]
    pub fn head(&self) -> Option<usize> {
        let done = self.done();
        let ahead = self.span.words - done.words;
        match self.activity {
            Activity::Hold => None,
            Activity::Window => (self.span.words > 0).then_some(self.span.start),
            Activity::Sweep { order: Order::Descending, .. } => (ahead > 0).then(|| done.start - 1),
            Activity::Sweep { .. } | Activity::Bus { .. } => (ahead > 0).then_some(done.end()),
        }
    }

    /// A bus phase's verify stream as a sweep of its own; `None` for any other phase and when the
    /// bus phase has no stream to verify.
    #[must_use]
    pub fn check_stream(&self) -> Option<Self> {
        match self.activity {
            Activity::Bus { check, checked, .. } if check.words > 0 => {
                Some(Self { span: check, activity: Activity::Sweep { order: Order::Ascending, done: checked } })
            },
            _ => None,
        }
    }
}
