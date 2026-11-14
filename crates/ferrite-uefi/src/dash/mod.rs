//! The framebuffer dashboard, laid out as a firmware setup screen, drawn into a [`Canvas`].
//!
//! A title bar and tab strip sit over flat sections for the system, progress, the pass's tests, a
//! memory map, the network and errors, then a log line and a key-hint footer.
//!
//! Each widget has a key, the few values it shows; a frame redraws only the widgets whose key
//! changed, and the canvas then draws only the parts of those that differ from what the widget drew
//! last, so the steady state pushes a few small rectangles to the screen. The page behind the
//! widgets is drawn only on a full redraw: the first frame, and when the screen size changes.

pub(crate) mod format;
mod layout;
mod theme;
mod widgets;

use core::time::Duration;

use ferrite_hal::Instant;
use ferrite_mem::analysis::Classification;
use ferrite_mem::eta::{BasisPoints, Eta, Position};
use ferrite_mem::suite::{Mode, Preset, TestKind};
use ferrite_net::NetStatus;
use snafu::{OptionExt as _, ResultExt as _, Snafu};

use self::layout::Layout;
use self::widgets::Pen;
use crate::gfx::text::{FontError, Fonts};
use crate::gfx::{Canvas, DirtyRects, Recording, Rect};
use crate::goal::Goal;
use crate::net::NoNetwork;
use crate::pace::{Delta, Overall};
use crate::report::{FaultRecord, Snapshot};
use crate::sweep::{Coverage, SweepStatus};
use crate::wall::{Ends, TimeLine, TimeOfDay};

/// Below this the layout has no room for its tables: three quarters of the 1024x768 design.
pub const MIN_SIZE: (u32, u32) = (768, 576);

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
    /// Why there is no network, when no port is up.
    pub net_err: Option<NoNetwork>,
    pub now: Instant,
    /// The time of day and when the times left end, when the clock has a source.
    pub time: Option<TimeLine>,
}

impl Frame<'_> {
    fn status(&self) -> &SweepStatus {
        &self.snapshot.status
    }

    fn elapsed(&self) -> Duration {
        self.snapshot.history.run_started.map_or(Duration::ZERO, |start| self.now - start)
    }

    /// The run against its goal, by the quarter second: the bar of a short goal moves that often,
    /// and the clock it prints, in whole seconds, still changes once a second.
    fn overall(&self) -> Overall {
        let quarters = self.elapsed().as_millis() / 250;
        self.status().outlook.overall(Duration::from_millis(u64::try_from(quarters * 250).unwrap_or(u64::MAX)))
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
}

/// The running test's figures that move: its rate now, its average and the average's change from
/// the saved time.
type Live = (Option<u64>, Option<u64>, Option<Delta>);
type SystemKey = (Preset, &'static str, usize, usize, Mode, usize, Coverage, (usize, usize), Option<u64>);
type PassKey = (u64, u64, bool, Option<Eta>, BasisPoints, (Option<BasisPoints>, Option<Eta>, u64));
type DetailKey = (Mode, usize, usize, u64, Live, Position, Option<u16>);
type TestsKey = (u64, (usize, usize), u64, bool, u64, Live);

/// What each widget shows; a widget is redrawn when its part differs from the last frame's.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Keys {
    setup: Setup,
    clock: (State, u64, u64, Option<(TimeOfDay, Option<Ends>)>),
    tabs: u64,
    system: SystemKey,
    pass: PassKey,
    test: ((usize, usize), u16, bool, BasisPoints, Position, Option<Duration>),
    detail: DetailKey,
    tests: TestsKey,
    map: ((usize, usize), Option<usize>, usize),
    net: (Option<NetStatus>, Option<NoNetwork>),
    errors: (u64, usize, Option<FaultRecord>, Classification, u64),
    log: (u64, &'static str, (usize, usize), u64, u64, u64, Goal),
    hints: bool,
}

impl Keys {
    fn of(f: &Frame<'_>, (width, height): (u32, u32)) -> Self {
        let s = f.status();
        let history = f.snapshot.history;
        let overall = f.overall();
        let live = (s.outlook.rate, s.outlook.test_avg, s.outlook.test_delta);
        Self {
            setup: Setup { width, height },
            clock: (f.state(), s.faults, f.elapsed().as_secs(), f.time.map(|time| (time.now, time.overall))),
            tabs: s.faults,
            system: (
                s.preset,
                s.tier,
                s.bytes,
                history.cpus.len(),
                s.mode,
                s.cpus,
                s.coverage,
                s.outlook.known,
                s.outlook.bandwidth,
            ),
            pass: (
                s.passes,
                f.pass_elapsed().as_secs(),
                s.paused,
                s.outlook.pass,
                s.outlook.pass_progress,
                (overall.fill, overall.left, overall.elapsed.as_secs()),
            ),
            test: (
                s.position,
                s.test_percent,
                s.paused,
                s.outlook.test_progress,
                s.outlook.position,
                s.outlook.test_left,
            ),
            detail: (
                s.mode,
                s.cpus,
                history.cpus.len(),
                history.revision,
                live,
                s.outlook.position,
                s.outlook.pace_percent,
            ),
            tests: (history.revision, s.position, s.passes, s.paused, f.snapshot.stats.revision, live),
            map: (s.position, (s.position.1 > 0).then(|| widgets::active_cell(s.test_percent)), s.bytes),
            net: (f.net, f.net_err),
            errors: (s.faults, f.snapshot.faults.len(), f.snapshot.faults.back().copied(), s.analysis, s.passes),
            log: (f.elapsed().as_secs(), s.test, s.position, s.passes, s.faults, history.revision, s.outlook.goal),
            hints: s.paused,
        }
    }

    /// Whether `widget` shows something other than in the frame with keys `last`.
    fn changed(&self, last: Option<&Self>, widget: Widget) -> bool {
        last.is_none_or(|last| match widget {
            Widget::Clock => last.clock != self.clock,
            Widget::Tabs => last.tabs != self.tabs,
            Widget::System => last.system != self.system,
            Widget::Pass => last.pass != self.pass,
            Widget::Test => last.test != self.test,
            Widget::Detail => last.detail != self.detail,
            Widget::Tests => last.tests != self.tests,
            Widget::Map => last.map != self.map,
            Widget::Net => last.net != self.net,
            Widget::Errors => last.errors != self.errors,
            Widget::Log => last.log != self.log,
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
    Clock,
    Tabs,
    System,
    Pass,
    Test,
    Detail,
    Tests,
    Map,
    Net,
    Errors,
    Log,
    Hints,
}

impl Widget {
    const ALL: [Self; 12] = [
        Self::Clock,
        Self::Tabs,
        Self::System,
        Self::Pass,
        Self::Test,
        Self::Detail,
        Self::Tests,
        Self::Map,
        Self::Net,
        Self::Errors,
        Self::Log,
        Self::Hints,
    ];

    const fn rect(self, l: &Layout) -> Rect {
        match self {
            Self::Clock => l.clock,
            Self::Tabs => l.tabs,
            Self::System => l.system.body,
            Self::Pass => l.pass,
            Self::Test => l.test,
            Self::Detail => l.detail,
            Self::Tests => l.tests_rows,
            Self::Map => l.map.body,
            Self::Net => l.net.body,
            Self::Errors => l.errors.body,
            Self::Log => l.log,
            Self::Hints => l.footer,
        }
    }

    const fn draw(self) -> Draw {
        match self {
            Self::Clock => widgets::clock,
            Self::Tabs => widgets::tabs,
            Self::System => widgets::system,
            Self::Pass => widgets::pass,
            Self::Test => widgets::test,
            Self::Detail => widgets::detail,
            Self::Tests => widgets::tests,
            Self::Map => widgets::map,
            Self::Net => widgets::net,
            Self::Errors => widgets::errors,
            Self::Log => widgets::log,
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
            layout: Layout::new(width, height),
            last: None,
            dirty: DirtyRects::new(bounds),
            drawn: core::array::from_fn(|_| Recording::roomy()),
        };
        dash.pen().backdrop();
        Ok(dash)
    }

    fn pen(&mut self) -> Pen<'_> {
        Pen { canvas: &mut self.canvas, fonts: &mut self.fonts, l: &self.layout }
    }

    /// Draw what changed since the last frame; [`Dashboard::output`] then has the rectangles to push.
    pub fn render(&mut self, f: &Frame<'_>) {
        self.dirty.clear();
        let bounds = self.canvas.bounds();
        let keys = Keys::of(f, (bounds.w, bounds.h));
        let last = self.last.filter(|last| last.setup == keys.setup);
        let full = last.is_none();
        if full {
            self.layout = Layout::new(bounds.w, bounds.h);
            self.pen().backdrop();
            self.dirty.add_all();
        }
        let l = self.layout;
        let band = (bounds.w / 24).max(64);
        for widget in Widget::ALL {
            if !keys.changed(last.as_ref(), widget) {
                continue;
            }
            let rect = widget.rect(&l);
            let mut pen = Pen { canvas: &mut self.canvas, fonts: &mut self.fonts, l: &l };
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
