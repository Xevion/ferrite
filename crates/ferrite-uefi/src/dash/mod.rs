//! The framebuffer dashboard: header, pass and test progress, the pass's tests, the fault log,
//! pass history, network and processors, drawn into a [`Canvas`].
//!
//! Each widget has a key, the few values it shows; a frame redraws only the widgets whose key
//! changed, and the canvas then draws only the parts of those that differ from what the widget drew
//! last, so the steady state pushes a few small rectangles to the screen. Cards and the page behind
//! them are drawn only on a full redraw: the first frame, and when the layout changes (the first
//! fault, a preset with a different number of tests).

mod format;
mod layout;
mod theme;
mod widgets;

use core::time::Duration;

use ferrite_hal::Instant;
use ferrite_mem::analysis::Classification;
use ferrite_mem::suite::{Mode, Preset, TestKind};
use ferrite_net::NetStatus;
use snafu::{OptionExt as _, ResultExt as _, Snafu};

use self::layout::Layout;
use self::widgets::Pen;
use crate::gfx::text::{FontError, Fonts};
use crate::gfx::{Canvas, DirtyRects, Recording, Rect};
use crate::report::{FaultRecord, Snapshot};
use crate::sweep::SweepStatus;

/// Below this the layout has no room for its tables.
pub const MIN_SIZE: (u32, u32) = (640, 480);

#[derive(Debug, Snafu)]
pub enum DashError {
    #[snafu(display("{width}x{height} is smaller than the dashboard's {}x{}", MIN_SIZE.0, MIN_SIZE.1))]
    TooSmall { width: u32, height: u32 },
    #[snafu(display("no memory for a {width}x{height} back buffer"))]
    NoBuffer { width: u32, height: u32 },
    #[snafu(display("loading fonts: {source}"))]
    Fonts { source: FontError },
}

/// What one frame shows.
pub struct Frame<'a> {
    pub snapshot: &'a Snapshot<'a>,
    pub net: Option<NetStatus>,
    /// Why there is no network, when setup failed.
    pub net_err: Option<&'static str>,
    pub now: Instant,
}

impl Frame<'_> {
    fn status(&self) -> &SweepStatus {
        &self.snapshot.status
    }

    fn elapsed(&self) -> Duration {
        self.snapshot.history.run_started.map_or(Duration::ZERO, |start| self.now - start)
    }

    fn pass_elapsed(&self) -> Duration {
        self.snapshot.history.pass_started.map_or(Duration::ZERO, |start| self.now - start)
    }

    fn state(&self) -> State {
        let status = self.status();
        if status.paused {
            State::Paused
        } else if status.position.1 == 0 {
            State::Starting
        } else {
            State::Running
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Starting,
    Running,
    Paused,
}

/// What the layout depends on; a frame whose setup differs from the last frame's redraws everything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Setup {
    width: u32,
    height: u32,
    /// Rows of the test table.
    tests: usize,
    /// Faults were found, which turns the header red and gives the fault log more room.
    faulted: bool,
}

/// What each widget shows; a widget is redrawn when its part differs from the last frame's.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Keys {
    setup: Setup,
    brand: (Preset, &'static str, usize, usize),
    clock: (State, u64, u64),
    pass: (u64, u16, u64, bool),
    test: (&'static str, (usize, usize), u16, Mode, usize, bool),
    tests: (u64, (usize, usize), u64, bool),
    faults: (u64, usize, Option<FaultRecord>, Classification, u64),
    net: (Option<NetStatus>, Option<&'static str>),
    passes: (u64, u64),
    cpus: (u64, Mode, usize),
    hints: bool,
}

impl Keys {
    fn of(f: &Frame<'_>, (width, height): (u32, u32)) -> Self {
        let s = f.status();
        let history = f.snapshot.history;
        Self {
            setup: Setup { width, height, tests: test_count(s.preset), faulted: s.faults > 0 },
            brand: (s.preset, s.tier, s.bytes, history.cpus.len()),
            clock: (f.state(), s.faults, f.elapsed().as_secs()),
            pass: (s.passes, s.percent, f.pass_elapsed().as_secs(), s.paused),
            test: (s.test, s.position, s.test_percent, s.mode, s.cpus, s.paused),
            tests: (history.revision, s.position, s.passes, s.paused),
            faults: (s.faults, f.snapshot.faults.len(), f.snapshot.faults.back().copied(), s.analysis, s.passes),
            net: (f.net, f.net_err),
            passes: (history.revision, s.passes),
            cpus: (history.revision, s.mode, s.cpus),
            hints: s.paused,
        }
    }

    /// Whether `widget` shows something other than in the frame with keys `last`.
    fn changed(&self, last: Option<&Self>, widget: Widget) -> bool {
        last.is_none_or(|last| match widget {
            Widget::Brand => last.brand != self.brand,
            Widget::Clock => last.clock != self.clock,
            Widget::Pass => last.pass != self.pass,
            Widget::Test => last.test != self.test,
            Widget::Tests => last.tests != self.tests,
            Widget::Faults => last.faults != self.faults,
            Widget::Net => last.net != self.net,
            Widget::Passes => last.passes != self.passes,
            Widget::Cpus => last.cpus != self.cpus,
            Widget::Hints => last.hints != self.hints,
        })
    }
}

fn test_count(preset: Preset) -> usize {
    TestKind::sequence(preset).count()
}

/// A widget's draw function.
type Draw = fn(&mut Pen<'_>, Rect, &Frame<'_>);

/// The parts of the screen redrawn on their own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Widget {
    Brand,
    Clock,
    Pass,
    Test,
    Tests,
    Faults,
    Net,
    Passes,
    Cpus,
    Hints,
}

impl Widget {
    const ALL: [Self; 10] = [
        Self::Brand,
        Self::Clock,
        Self::Pass,
        Self::Test,
        Self::Tests,
        Self::Faults,
        Self::Net,
        Self::Passes,
        Self::Cpus,
        Self::Hints,
    ];

    const fn rect(self, l: &Layout) -> Rect {
        match self {
            Self::Brand => l.brand,
            Self::Clock => l.clock,
            Self::Pass => l.pass,
            Self::Test => l.test,
            Self::Tests => l.tests,
            Self::Faults => l.faults,
            Self::Net => l.net,
            Self::Passes => l.passes,
            Self::Cpus => l.cpus,
            Self::Hints => l.footer,
        }
    }

    const fn draw(self) -> Draw {
        match self {
            Self::Brand => widgets::brand,
            Self::Clock => widgets::clock,
            Self::Pass => widgets::pass,
            Self::Test => widgets::test,
            Self::Tests => widgets::tests,
            Self::Faults => widgets::faults,
            Self::Net => widgets::net,
            Self::Passes => widgets::passes,
            Self::Cpus => widgets::cpus,
            Self::Hints => widgets::hints,
        }
    }
}

pub struct Dashboard {
    canvas: Canvas,
    fonts: Fonts,
    layout: Layout,
    last: Option<Keys>,
    dirty: DirtyRects,
    /// What each widget drew last, by [`Widget`].
    drawn: [Recording; Widget::ALL.len()],
}

impl Dashboard {
    pub fn new(width: u32, height: u32) -> Result<Self, DashError> {
        if width < MIN_SIZE.0 || height < MIN_SIZE.1 {
            return TooSmallSnafu { width, height }.fail();
        }
        let canvas = Canvas::new(width, height).context(NoBufferSnafu { width, height })?;
        let fonts = Fonts::load().context(FontsSnafu)?;
        let bounds = canvas.bounds();
        let mut dash = Self {
            canvas,
            fonts,
            layout: Layout::new(width, height, test_count(Preset::Standard), false),
            last: None,
            dirty: DirtyRects::new(bounds),
            drawn: core::array::from_fn(|_| Recording::roomy()),
        };
        dash.pen(false).backdrop();
        Ok(dash)
    }

    fn pen(&mut self, faulted: bool) -> Pen<'_> {
        Pen { canvas: &mut self.canvas, fonts: &mut self.fonts, l: &self.layout, faulted }
    }

    /// Draw what changed since the last frame; [`Dashboard::output`] then has the rectangles to push.
    pub fn render(&mut self, f: &Frame<'_>) {
        self.dirty.clear();
        let bounds = self.canvas.bounds();
        let keys = Keys::of(f, (bounds.w, bounds.h));
        let faulted = keys.setup.faulted;
        let last = self.last.filter(|last| last.setup == keys.setup);
        let full = last.is_none();
        if full {
            self.layout = Layout::new(bounds.w, bounds.h, keys.setup.tests, faulted);
            self.pen(faulted).backdrop();
            self.dirty.add_all();
        }
        let l = self.layout;
        let band = (bounds.w / 24).max(64);
        for widget in Widget::ALL {
            if !keys.changed(last.as_ref(), widget) {
                continue;
            }
            let rect = widget.rect(&l);
            let mut pen = Pen { canvas: &mut self.canvas, fonts: &mut self.fonts, l: &l, faulted };
            pen.begin(rect);
            widget.draw()(&mut pen, rect, f);
            self.canvas.finish(&mut self.fonts, &mut self.drawn[widget as usize], full, &mut self.dirty, band);
        }
        self.last = Some(keys);
    }

    /// Redraw everything next frame, after a push that may have left the screen half drawn.
    pub fn invalidate(&mut self) {
        self.last = None;
    }

    /// The canvas and the rectangles of it the last [`Dashboard::render`] changed.
    #[must_use]
    pub fn output(&self) -> (&Canvas, &[Rect]) {
        (&self.canvas, self.dirty.as_slice())
    }
}

#[cfg(test)]
mod bench;
#[cfg(test)]
mod tests;
