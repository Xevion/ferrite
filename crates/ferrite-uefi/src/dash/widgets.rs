//! The static chrome behind the widgets, and each widget's drawing. A widget draws only inside its
//! own rectangle, starting from a cleared ground, so it can be redrawn alone.

#![expect(
    clippy::many_single_char_names,
    reason = "p, r, f, s and l are the pen, rectangle, frame, status and layout in every widget"
)]

use ferrite_mem::eta::{BasisPoints, Confidence, Position};
use ferrite_mem::suite::{Mode, TestKind};
use ferrite_net::{Medium, PortStatus};
use heapless::String;

use super::format::{self, Text, addr, fixed, hms, num, plural};
use super::layout::{LEGEND_ROWS, Layout, MAP_COLS, MAP_ROWS, Section, window};
use super::theme::{
    ACTIVE, ACTIVE_RING, BAR, BAR_DIM, BAR_RULE, BAR_TEXT, BG, DIM, ERR, FAULT, FILL, FIRMWARE, HELD, HOLDING, KEY,
    KEY_INK, LABEL, LINE, PASS, PENDING, SEL, SEL_INK, STRONG, TAB_ON, TAB_ON_INK, TAB_TEXT, TESTED, TEXT, TRACK, WARN,
};
use super::{Frame, Notice, State};
use crate::gfx::text::{Case, Face, Fonts, Style};
use crate::gfx::{Canvas, Rect, Rgb, px, whole_px};
use crate::memmap::CellState;
use crate::pace::Steadiness;
use crate::report::FaultRecord;

/// The placeholder for what this build cannot read from the machine.
const NONE: &str = "\u{2014}";
const SEPARATOR: &str = "\u{b7}";

/// Cells of the memory map.
pub const CELLS: usize = (MAP_COLS * MAP_ROWS) as usize;

/// The text after a progress bar: `head` in the plain ink, then `tail` in `ink`.
#[derive(Clone, Copy)]
struct Figure<'a> {
    head: &'a str,
    tail: &'a str,
    ink: Rgb,
}

/// The ink of a time left: plain when measured, dim when it is a guess.
const fn confidence_ink(confidence: Confidence) -> Rgb {
    match confidence {
        Confidence::Measured => TEXT,
        Confidence::Estimated => DIM,
    }
}

pub struct Pen<'d> {
    pub canvas: &'d mut Canvas,
    pub fonts: &'d mut Fonts,
    pub l: &'d Layout,
    /// What the log line says in place of the latest test, while it is on show.
    pub notice: Option<&'d Notice>,
}

impl Pen<'_> {
    /// Start drawing the widget in `r`, which is cleared to what lies behind it and drawn only in.
    pub fn begin(&mut self, r: Rect) {
        let bars = [self.l.header, self.l.tabs, self.l.footer];
        let ground = if bars.iter().any(|bar| bar.intersect(r) == r) { BAR } else { BG };
        self.canvas.begin(r, ground);
    }

    /// The page, the title bar and the section labels.
    pub fn backdrop(&mut self) {
        let l = *self.l;
        let all = self.canvas.bounds();
        self.canvas.fill(all, BG);
        self.canvas.fill(l.header, BAR);
        self.brand();
        let titled = [
            (l.system, "SYSTEM"),
            (l.progress, "PROGRESS"),
            (l.tests, "TESTS"),
            (l.map, "MEMORY MAP"),
            (l.net, "NETWORK"),
            (l.errors, "ERRORS"),
        ];
        for (section, title) in titled {
            self.section_label(section, title);
        }
        self.dashes(l.detail_rule);
        self.tests_head();
        self.canvas.fill(l.log_rule, LINE);
    }

    fn brand(&mut self) {
        let l = *self.l;
        let h = l.header;
        let name = Style { tracking: l.m(3.0), ..Self::style(Face::Bold, l.text.title, BAR_TEXT) };
        let base = self.mid(l.text.title, h.y, h.h);
        let mut x = px(h.x) + l.m(20.0);
        x += self.text(x, base, "FERRITE", name) + l.m(14.0);
        x += self.text(x, base, "Memory Diagnostic", Self::style(Face::Regular, l.text.base, BAR_TEXT)) + l.m(14.0);
        self.text(x, base, concat!("v", env!("CARGO_PKG_VERSION")), Self::style(Face::Regular, l.text.base, BAR_DIM));
    }

    fn section_label(&mut self, s: Section, title: &str) {
        let l = *self.l;
        let base = self.mid(l.text.label, s.label.y, l.row_h - l.row_h / 8);
        let style = self.label_style(LABEL);
        self.canvas.text(self.fonts, px(s.label.x), base, title, style);
        self.canvas.fill(Rect::new(s.label.x, s.label.bottom() - 1, s.label.w, 1), LINE);
    }

    /// A one pixel rule of three pixel dashes.
    fn dashes(&mut self, r: Rect) {
        let dash = whole_px(self.l.m(3.0)).max(2);
        let mut x = r.x;
        while x < r.right() {
            self.canvas.fill(Rect::new(x, r.y, dash.min(r.right() - x), 1), LINE);
            x += 2 * dash;
        }
    }

    const fn style(face: Face, px: u16, color: Rgb) -> Style {
        Style { face, px, color, tracking: 0.0, case: Case::AsWritten }
    }

    fn body(&self, color: Rgb) -> Style {
        Self::style(Face::Regular, self.l.text.base, color)
    }

    /// The face of the tests table's cells.
    fn table(&self, color: Rgb) -> Style {
        Self::style(Face::Regular, self.l.text.table, color)
    }

    fn label_style(&self, color: Rgb) -> Style {
        let px = self.l.text.label;
        Style { face: Face::SemiBold, px, color, tracking: self.l.m(2.0), case: Case::Upper }
    }

    /// The baseline that centres capitals at `px_size` in the band `top..top + h`.
    fn mid(&self, px_size: u16, top: u32, h: u32) -> f32 {
        self.fonts.centred_baseline(px_size, px(top), px(h))
    }

    fn text(&mut self, x: f32, base: f32, text: &str, style: Style) -> f32 {
        self.canvas.text(self.fonts, x, base, text, style)
    }

    fn fit(&mut self, x: f32, base: f32, text: &str, style: Style, max: f32) -> f32 {
        if max <= 0.0 {
            return 0.0;
        }
        self.canvas.text_fit(self.fonts, x, base, text, style, max)
    }

    fn right(&mut self, right: f32, base: f32, text: &str, style: Style) -> f32 {
        self.canvas.text_right(self.fonts, right, base, text, style)
    }

    fn measure(&mut self, text: &str, style: Style) -> f32 {
        self.fonts.measure(text, style)
    }

    /// A square-ended progress bar: a bordered track and its fill, always the same three steps so
    /// a growing fill redraws only the columns it gained.
    fn bar(&mut self, r: Rect, progress: BasisPoints, fill: Rgb) {
        let inner = Rect::new(r.x + 1, r.y + 1, r.w.saturating_sub(2), r.h.saturating_sub(2));
        self.canvas.fill(r, LINE);
        self.canvas.fill(inner, TRACK);
        let filled = progress.of_whole(inner.w);
        self.canvas.fill(Rect::new(inner.x, inner.y, filled, inner.h), fill);
    }

    /// One line: a label column, a bar and a right-aligned figure of two parts, such as `41.37% ·`
    /// and `12:34 left`, the second in its own ink: dim when it is only a guess.
    fn bar_row(&mut self, row: Rect, label: &str, figure: Figure<'_>, progress: BasisPoints, fill: Rgb) {
        let l = *self.l;
        let gap = whole_px(l.m(12.0));
        let base = self.mid(l.text.base, row.y, row.h);
        self.fit(px(row.x), base, label, self.body(DIM), px(l.lab));
        let bar_h = whole_px(l.m(16.0)).min(row.h);
        let x = row.x + l.lab + gap;
        let w = row.w.saturating_sub(l.lab + l.pw + 2 * gap);
        self.bar(Rect::new(x, row.y + (row.h - bar_h) / 2, w, bar_h), progress, fill);
        let tail = self.right(px(row.right()), base, figure.tail, self.body(figure.ink));
        self.right(px(row.right()) - tail, base, figure.head, self.body(TEXT));
    }

    /// Pairs of a dim label and a value, two pairs to a row, over `rows` rows.
    fn pairs(&mut self, r: Rect, cells: &[(&str, &str)]) {
        let l = *self.l;
        let (lab, gap) = (px(l.lab), l.m(12.0));
        let value_w = (px(r.w) - 2.0 * lab - 3.0 * gap) / 2.0;
        for (n, (key, value)) in cells.iter().enumerate() {
            let (row, col) = (u32::try_from(n / 2).unwrap_or(0), u32::try_from(n % 2).unwrap_or(0));
            let x = px(r.x) + px(col) * (lab + gap + value_w + gap);
            let base = self.mid(l.text.base, r.y + row * l.pitch, l.row_h);
            self.fit(x, base, key, self.body(DIM), lab);
            let color = if *value == NONE { DIM } else { TEXT };
            self.fit(x + lab + gap, base, value, self.body(color), value_w);
        }
    }

    /// A label and a value on row `row` of `r`.
    fn line(&mut self, r: Rect, row: u32, key: &str, value: &str, color: Rgb) -> f32 {
        let l = *self.l;
        let key_w = l.m(100.0);
        let base = self.mid(l.text.base, r.y + row * l.pitch, l.row_h);
        self.fit(px(r.x), base, key, self.body(DIM), key_w);
        let x = px(r.x) + key_w;
        x + self.fit(x, base, value, self.body(color), px(r.right()) - x)
    }

    fn tests_head(&mut self) {
        let l = *self.l;
        let head = l.tests_head;
        let cols = TestCols::of(&l, l.tests_rows);
        let base = self.mid(l.text.label, head.y, head.h.saturating_sub(4));
        let style = Style { tracking: 0.0, ..self.label_style(DIM) };
        let style = Style { face: Face::Regular, ..style };
        self.text(cols.num_x, base, "#", style);
        self.text(cols.name_x, base, "NAME", style);
        self.text(cols.status_x, base, "STATUS", style);
        let steady = if l.median { "MEDIAN" } else { "SPREAD" };
        for (right, title) in
            [(cols.errors_r, "ERRORS"), (cols.rate_r, "RATE"), (cols.median_r, steady), (cols.time_r, "TIME")]
        {
            let w = self.measure(title, style);
            self.text(right - w, base, title, style);
        }
        self.canvas.fill(Rect::new(head.x, head.bottom() - 1, head.w, 1), LINE);
    }
}

/// Where the tests table's columns go within `r`.
struct TestCols {
    num_x: f32,
    name_x: f32,
    name_max: f32,
    status_x: f32,
    errors_r: f32,
    rate_r: f32,
    median_r: f32,
    time_r: f32,
    /// Widths of the room a rate's change and a median's spread have at the right of their columns.
    change_w: f32,
    spread_w: f32,
}

impl TestCols {
    fn of(l: &Layout, r: Rect) -> Self {
        let [num_w, status_w, errors_w, rate_w, median_w, time_w] = l.cols.map(px);
        let em = 0.6 * f32::from(l.text.table);
        let compact = l.text.table < l.text.base;
        let (pad, gap) = (l.m(if compact { 6.0 } else { 14.0 }), l.m(if compact { 6.0 } else { 8.0 }));
        let num_x = px(r.x) + pad;
        let name_x = num_x + num_w + gap;
        let time_r = px(r.right()) - pad;
        let median_r = time_r - time_w - gap;
        let rate_r = median_r - median_w - gap;
        let errors_r = rate_r - rate_w - gap;
        let status_x = errors_r - errors_w - gap - status_w;
        Self {
            num_x,
            name_x,
            name_max: status_x - gap - name_x,
            status_x,
            errors_r,
            rate_r,
            median_r,
            time_r,
            change_w: 5.0 * em,
            spread_w: 6.0 * em,
        }
    }
}

/// The gap between the title bar's figures, in design pixels.
const CLOCK_GAP: f32 = 22.0;

/// The style of the title bar's figures before their emphasis.
fn clock_style(l: &Layout) -> Style {
    Style { tracking: l.m(0.5), ..Pen::style(Face::Regular, l.text.label, BAR_DIM) }
}

pub fn clock(p: &mut Pen<'_>, r: Rect, f: &Frame<'_>) {
    let s = f.status();
    let l = *p.l;
    let base = p.mid(l.text.label, r.y, r.h);
    let plain = clock_style(&l);
    let (word, loud) = match f.state() {
        State::Starting => ("STARTING", false),
        State::Running => ("RUNNING", false),
        State::Paused => ("PAUSED", true),
    };
    let state = if s.faults > 0 {
        fixed::<48>(format_args!("{} ERROR{} {SEPARATOR} {word}", s.faults, if s.faults == 1 { "" } else { "S" }))
    } else {
        fixed(format_args!("{word}"))
    };
    let state_style = if loud || s.faults > 0 { Style { face: Face::Bold, color: BAR_TEXT, ..plain } } else { plain };
    let mut right = px(r.right());
    right -= p.right(right, base, &state, state_style) + l.m(CLOCK_GAP);
    let bright = Style { color: BAR_TEXT, ..plain };
    right -= p.right(right, base, &fixed::<32>(format_args!("ELAPSED {}", hms(f.elapsed()))), bright);
    if let Some(time) = f.time {
        let now = time.now.text(time.format);
        // The end time is the first thing given up when the bar runs out of room.
        if let Some(ends) = time.overall {
            let ends = fixed::<32>(format_args!("ENDS {}", ends.text(time.format)));
            let need = p.measure(&ends, bright) + p.measure(&now, bright) + 2.0 * l.m(CLOCK_GAP);
            if right - need >= px(r.x) {
                right -= l.m(CLOCK_GAP);
                right -= p.right(right, base, &ends, bright);
            }
        }
        right -= l.m(CLOCK_GAP);
        p.right(right, base, &now, bright);
    }
}

pub fn tabs(p: &mut Pen<'_>, r: Rect, f: &Frame<'_>) {
    let s = f.status();
    let l = *p.l;
    p.canvas.fill(Rect::new(r.x, r.y, r.w, 1), BAR_RULE);
    let rule = whole_px(l.m(2.0)).max(1);
    p.canvas.fill(Rect::new(r.x, r.bottom() - rule, r.w, rule), TAB_ON);
    let results =
        if s.faults > 0 { fixed::<24>(format_args!("Results ({})", s.faults)) } else { fixed(format_args!("Results")) };
    let labels: [&str; 4] = ["Run", &results, "Setup", "Log"];
    let (pad, gap) = (l.m(18.0), l.m(2.0));
    let top = r.y + 1;
    let height = r.h.saturating_sub(1 + rule);
    let mut x = px(r.x) + l.m(20.0);
    for (i, label) in labels.into_iter().enumerate() {
        let on = i == 0;
        let style = Pen::style(
            if on { Face::Bold } else { Face::Regular },
            l.text.base,
            if on { TAB_ON_INK } else { TAB_TEXT },
        );
        let w = p.measure(label, style) + 2.0 * pad;
        if on {
            p.canvas.fill(Rect::new(whole_px(x), top, whole_px(w), height), TAB_ON);
        }
        let base = p.mid(l.text.base, top, height);
        p.text(x + pad, base, label, style);
        x += w + gap;
    }
}

pub fn system(p: &mut Pen<'_>, r: Rect, f: &Frame<'_>) {
    let s = f.status();
    let list = &f.snapshot.history.cpus;
    let facts = &f.hardware.facts;
    let cores = (0..list.len())
        .filter(|&i| !list[..i].iter().any(|before| (before.package, before.core) == (list[i].package, list[i].core)))
        .count();
    let cpu: String<48> = match (facts.cpu_short.as_str(), list.is_empty()) {
        ("", true) => fixed(format_args!("1 logical")),
        ("", false) => fixed(format_args!("{} logical {SEPARATOR} {cores} core{}", list.len(), plural(cores))),
        (name, true) => fixed(format_args!("{name}")),
        (name, false) => fixed(format_args!("{name} {SEPARATOR} {cores}C/{}T", list.len())),
    };
    let memory: String<48> = if s.coverage.free > 0 {
        let (tested, free) =
            (usize::try_from(s.coverage.tested).unwrap_or(0), usize::try_from(s.coverage.free).unwrap_or(0));
        fixed(format_args!("{} of {} free ({}%)", format::size(tested), format::size(free), s.coverage.percent()))
    } else {
        fixed(format_args!("{} under test", format::size(s.bytes)))
    };
    let preset: &str = s.preset.into();
    let bandwidth = s
        .outlook
        .bandwidth
        .map_or_else(|| fixed::<16>(format_args!("{NONE}")), |b| fixed(format_args!("{}", format::rate(b))));
    let timings: String<32> = match s.outlook.known {
        (_, 0) => fixed(format_args!("{NONE}")),
        (0, n) => fixed(format_args!("learning, 0 of {n}")),
        (known, n) => fixed(format_args!("{known} of {n} known")),
    };
    let modules: String<48> = if facts.modules.is_empty() {
        fixed(format_args!("{NONE}"))
    } else {
        let ecc = if facts.corrects_errors() == Some(true) { " ECC" } else { "" };
        fixed(format_args!("{}{ecc}", facts.modules))
    };
    let live = f.hardware.vitals();
    let clock: String<32> = match (live.mhz, live.celsius) {
        (None, None) => fixed(format_args!("{NONE}")),
        (None, Some(c)) => fixed(format_args!("{c} C")),
        (Some(mhz), None) => fixed(format_args!("{}{mhz} MHz", if live.measured { "" } else { "base " })),
        (Some(mhz), Some(c)) => {
            fixed(format_args!("{}{mhz} MHz {SEPARATOR} {c} C", if live.measured { "" } else { "base " }))
        },
    };
    p.pairs(
        r,
        &[
            ("CPU", &cpu),
            ("Memory", &memory),
            ("Preset", preset),
            ("Tier", s.tier),
            ("Modules", &modules),
            ("Timings", &timings),
            ("Bandwidth", &bandwidth),
            ("Clock", &clock),
        ],
    );
}

fn fill_of(paused: bool) -> Rgb {
    if paused { HELD } else { FILL }
}

pub fn pass(p: &mut Pen<'_>, r: Rect, f: &Frame<'_>) {
    let s = f.status();
    let o = &s.outlook;
    let l = *p.l;
    let gap = l.m(8.0);
    let overall = f.overall();
    let rect = Rect::new(r.x, r.y, r.w, l.row_h);
    let stop_on_fault = o.goal.stop_on_fault && o.goal.passes.is_none() && o.goal.time.is_none();
    let (head, tail, ink): (String<24>, Text, Rgb) = match (overall.fill, overall.left) {
        (Some(fill), Some(left)) => {
            (fixed(format_args!("{fill} {SEPARATOR} ")), format::left(left.remaining), confidence_ink(left.confidence))
        },
        (Some(fill), None) => (fixed(format_args!("{fill} {SEPARATOR} ")), format::clock(overall.elapsed), TEXT),
        (None, _) if stop_on_fault => {
            (fixed(format_args!("first fault {SEPARATOR} ")), format::clock(overall.elapsed), TEXT)
        },
        (None, _) => (fixed(format_args!("no goal {SEPARATOR} ")), format::clock(overall.elapsed), TEXT),
    };
    let fill = overall.fill.unwrap_or(BasisPoints::ZERO);
    p.bar_row(rect, "Overall", Figure { head: &head, tail: &tail, ink }, fill, fill_of(s.paused));
    let pass = Rect::new(r.x, r.y + l.row_h + whole_px(gap), r.w, l.row_h);
    let head: String<24> = fixed(format_args!("{} {SEPARATOR} ", o.pass_progress));
    let (tail, ink) = o.pass.map_or_else(
        || (format::clock(f.pass_elapsed()), TEXT),
        |eta| (format::left(eta.remaining), confidence_ink(eta.confidence)),
    );
    let label = fixed::<24>(format_args!("Pass {}", s.passes + 1));
    p.bar_row(pass, &label, Figure { head: &head, tail: &tail, ink }, o.pass_progress, fill_of(s.paused));
}

pub fn test(p: &mut Pen<'_>, r: Rect, f: &Frame<'_>) {
    let s = f.status();
    let o = &s.outlook;
    let (i, n) = s.position;
    if n == 0 {
        p.bar_row(r, "Test", Figure { head: "", tail: NONE, ink: TEXT }, BasisPoints::ZERO, fill_of(s.paused));
        return;
    }
    let label = fixed::<24>(format_args!("Test {i}/{n}"));
    let progress = o.test_progress;
    let (head, tail): (String<24>, Text) = match (o.position, o.test_left) {
        (Position::Wait { nth, waits, left }, _) => {
            (String::new(), fixed(format_args!("wait {nth}/{waits} {SEPARATOR} {}", format::clock(left))))
        },
        (Position::Work { .. }, Some(left)) => (fixed(format_args!("{progress} {SEPARATOR} ")), format::clock(left)),
        (Position::Work { .. }, None) => (String::new(), fixed(format_args!("{progress}"))),
    };
    p.bar_row(r, &label, Figure { head: &head, tail: &tail, ink: TEXT }, progress, fill_of(s.paused));
}

pub fn detail(p: &mut Pen<'_>, r: Rect, f: &Frame<'_>) {
    let s = f.status();
    let o = &s.outlook;
    let total = f.snapshot.history.cpus.len().max(s.cpus).max(1);
    let threads: String<48> = match s.mode {
        Mode::Split => fixed(format_args!("{} / {total} parallel", s.cpus.min(total))),
        Mode::Single => fixed(format_args!("1 / {total} {SEPARATOR} single core")),
    };
    let rate: String<48> = match (o.position, o.rate) {
        (Position::Wait { nth, waits, left }, _) => {
            fixed(format_args!("idle, wait {nth}/{waits} {SEPARATOR} {}", format::clock(left)))
        },
        (Position::Work { .. }, Some(rate)) if !s.paused => format::rate(rate).as_str().try_into().unwrap_or_default(),
        (Position::Work { .. }, _) => fixed(format_args!("{NONE}")),
    };
    let average: String<48> = match (o.test_avg, o.test_delta) {
        (Some(avg), Some(delta)) => fixed(format_args!("{} {delta}", format::rate(avg))),
        (Some(avg), None) => fixed(format_args!("{}", format::rate(avg))),
        (None, _) => fixed(format_args!("{NONE}")),
    };
    let pace: String<16> =
        o.pace_percent.map_or_else(|| fixed(format_args!("{NONE}")), |p| fixed(format_args!("{p}%")));
    p.pairs(
        r,
        &[
            ("Range", NONE),
            ("Threads", &threads),
            ("Pattern", NONE),
            ("Rate now", &rate),
            ("Test avg", &average),
            ("Pace", &pace),
        ],
    );
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Row {
    Done,
    Current,
    Pending,
}

/// A figure and what goes with it at the right of its column: the rate and its change from the
/// saved one, or the median and its spread, each in its own room so the columns line up.
struct Pair<'a> {
    main: &'a str,
    main_ink: Rgb,
    extra: &'a str,
    extra_ink: Rgb,
}

fn pair_cell(p: &mut Pen<'_>, base: f32, right: f32, room: f32, pair: &Pair<'_>) {
    p.right(right, base, pair.extra, p.table(pair.extra_ink));
    p.right(right - room, base, pair.main, p.table(pair.main_ink));
}

pub fn tests(p: &mut Pen<'_>, r: Rect, f: &Frame<'_>) {
    let s = f.status();
    let o = &s.outlook;
    let l = *p.l;
    let current = s.position.0.checked_sub(1).filter(|_| s.position.1 > 0);
    let cols = TestCols::of(&l, r);
    let visible = (r.h / l.pitch.max(1)) as usize;
    let start = window(super::test_count(s.preset), visible, current.unwrap_or(0));
    for (slot, (i, kind)) in TestKind::sequence(s.preset).enumerate().skip(start).take(visible).enumerate() {
        let top = r.y + u32::try_from(slot).unwrap_or(0) * l.pitch;
        let row = match current {
            Some(c) if i < c => Row::Done,
            Some(c) if i == c => Row::Current,
            _ => Row::Pending,
        };
        let record = (row == Row::Done)
            .then(|| f.snapshot.history.tests.iter().rev().find(|rec| rec.test == kind.name()))
            .flatten();
        let finished = (row == Row::Done).then(|| f.snapshot.stats.get(i, kind.name())).flatten();
        if row == Row::Current {
            p.canvas.fill(Rect::new(r.x, top, r.w, l.pitch), SEL);
        }
        let selected = row == Row::Current;
        let ink = |color: Rgb| if selected { SEL_INK } else { color };
        let base = p.mid(l.text.table, top, l.pitch);
        p.text(cols.num_x, base, &num(i + 1), p.table(ink(DIM)));
        let name_color = if row == Row::Pending { TEXT } else { STRONG };
        p.fit(cols.name_x, base, kind.name(), p.table(ink(name_color)), cols.name_max);

        let unsteady = finished.and_then(|st| st.steadiness).is_some_and(Steadiness::unsteady);
        let (status, status_color) = match (row, record) {
            (Row::Current, _) if s.paused => ("PAUSED", SEL_INK),
            (Row::Current, _) => ("RUNNING", SEL_INK),
            (Row::Pending, _) => ("QUEUED", DIM),
            (Row::Done, Some(rec)) if rec.faults > 0 => ("FAIL", ERR),
            (Row::Done, Some(_)) => ("PASS", PASS),
            (Row::Done, None) => ("DONE", DIM),
        };
        let status_style = Pen::style(Face::SemiBold, l.text.table, status_color);
        p.text(cols.status_x, base, status, status_style);

        match record {
            Some(rec) if rec.faults > 0 => {
                p.right(cols.errors_r, base, &num(rec.faults), p.table(ERR));
            },
            Some(_) => {
                p.right(cols.errors_r, base, "0", p.table(TEXT));
            },
            None => {
                p.right(cols.errors_r, base, NONE, p.table(ink(TEXT)));
            },
        }

        // The rate: what the test averaged over its work time, or while it runs, how fast it is
        // going now (its average while it waits), with how it compares with the saved time.
        let (rate, delta) = match row {
            Row::Done => (finished.and_then(|st| st.rate), finished.and_then(|st| st.delta)),
            Row::Current if s.paused => (None, None),
            Row::Current => (o.rate.or(o.test_avg), o.test_delta),
            Row::Pending => (None, None),
        };
        let delta_ink = match delta {
            _ if selected => SEL_INK,
            Some(d) if d.percent() < 0 => WARN,
            _ => PASS,
        };
        let main: Text = rate.map_or_else(|| fixed(format_args!("{NONE}")), format::rate);
        let extra: String<8> = delta.map_or_else(String::new, |d| fixed(format_args!("{d}")));
        let main_ink = if rate.is_some() { ink(TEXT) } else { ink(DIM) };
        pair_cell(
            p,
            base,
            cols.rate_r,
            cols.change_w,
            &Pair { main: &main, main_ink, extra: &extra, extra_ink: delta_ink },
        );

        // The median of the per-second rates and how far they strayed from it.
        let steadiness = finished.and_then(|st| st.steadiness);
        let main: Text = steadiness.map_or_else(|| fixed(format_args!("{NONE}")), |st| format::rate(st.median));
        let extra: Text = steadiness.map_or_else(Text::new, |st| format::spread(st.spread));
        let main_ink = if steadiness.is_some() { ink(TEXT) } else { ink(DIM) };
        let extra_ink = if unsteady { WARN } else { ink(DIM) };
        if l.median {
            pair_cell(p, base, cols.median_r, cols.spread_w, &Pair { main: &main, main_ink, extra: &extra, extra_ink });
        } else if steadiness.is_some() {
            p.right(cols.median_r, base, &extra, p.table(extra_ink));
        } else {
            p.right(cols.median_r, base, NONE, p.table(ink(DIM)));
        }

        let took = record.map_or_else(|| fixed(format_args!("{NONE}")), |rec| format::took(rec.took));
        p.right(cols.time_r, base, &took, p.table(ink(DIM)));
    }
}

/// A cell of the memory map: the same two steps for every cell, so a cell changing state redraws
/// only itself. A ring is the outer step in the ring's colour.
fn cell(p: &mut Pen<'_>, r: Rect, color: Rgb, ring: Option<(Rgb, u32)>) {
    if let Some((ring, width)) = ring {
        p.canvas.fill(r, ring);
        let inner = Rect::new(r.x + width, r.y + width, r.w.saturating_sub(2 * width), r.h.saturating_sub(2 * width));
        p.canvas.fill(inner, color);
    } else {
        p.canvas.fill(r, color);
        p.canvas.fill(r, color);
    }
}

/// What a cell in `state` looks like: its colour and its outline, a ring in the outline's colour
/// and its width.
fn look(state: CellState, ring: u32) -> (Rgb, Option<(Rgb, u32)>) {
    match state {
        CellState::NoRam => (BG, Some((LINE, 1))),
        CellState::Firmware => (FIRMWARE, None),
        CellState::Pending => (PENDING, None),
        CellState::Done => (TESTED, None),
        CellState::Holding => (HOLDING, None),
        CellState::Active => (ACTIVE, Some((ACTIVE_RING, ring))),
        CellState::Fault => (FAULT, None),
    }
}

/// The name of a state in the memory map's legend.
const fn legend_name(state: CellState) -> &'static str {
    match state {
        CellState::NoRam => "No RAM",
        CellState::Firmware => "Firmware",
        CellState::Pending => "Pending",
        CellState::Done => "Tested",
        CellState::Holding => "Holding",
        CellState::Active => "Active",
        CellState::Fault => "Fault",
    }
}

/// The states in the order the legend lists them.
const LEGEND: [CellState; 7] = [
    CellState::Done,
    CellState::Active,
    CellState::Pending,
    CellState::Holding,
    CellState::Fault,
    CellState::Firmware,
    CellState::NoRam,
];

pub fn map(p: &mut Pen<'_>, r: Rect, f: &Frame<'_>) {
    let map = &f.status().map;
    let l = *p.l;
    let small = Pen::style(Face::Regular, l.text.small, DIM);
    let base = p.mid(l.text.small, r.y, l.row_h);
    p.text(px(r.x), base, "0", small);
    if map.top > 0 {
        let top = usize::try_from(map.top).unwrap_or(usize::MAX);
        let size_w = p.right(px(r.right()), base, &format::size(top), small);
        let each = fixed::<32>(format_args!(
            "1 cell = {}",
            format::size(usize::try_from(map.cell_bytes()).unwrap_or(usize::MAX))
        ));
        let w = p.measure(&each, small);
        if let Some(caption) = f.hardware.facts.caption.as_deref() {
            // The cell size moves up against the map's size to leave the caption the room after the 0.
            let gap = l.m(10.0);
            let after_zero = px(r.x) + p.measure("0", small) + gap;
            let each_x = px(r.right()) - size_w - gap - w;
            p.text(each_x, base, &each, small);
            p.fit(after_zero, base, caption, small, each_x - gap - after_zero);
        } else {
            p.text(px(r.x) + (px(r.w) - w) / 2.0, base, &each, small);
        }
    } else {
        p.right(px(r.right()), base, NONE, small);
    }

    let (gap, cell_h) = (whole_px(l.m(3.0)), whole_px(l.m(20.0)));
    let top = r.y + l.row_h + whole_px(l.m(6.0));
    let ring = whole_px(l.m(2.0)).max(1);
    for (i, &state) in map.cells.iter().enumerate() {
        let (col, row) =
            (u32::try_from(i % MAP_COLS as usize).unwrap_or(0), u32::try_from(i / MAP_COLS as usize).unwrap_or(0));
        let x0 = r.x + col * (r.w + gap) / MAP_COLS;
        let x1 = r.x + (col + 1) * (r.w + gap) / MAP_COLS - gap;
        let rect = Rect::new(x0, top + row * (cell_h + gap), x1 - x0, cell_h);
        let (color, edge) = look(state, ring);
        cell(p, rect, color, edge);
    }
    if map.top == 0 {
        return;
    }

    let legend_y = top + MAP_ROWS * (cell_h + gap) - gap + whole_px(l.m(10.0));
    let (line_h, side) = (whole_px(l.m(16.0)), whole_px(l.m(12.0)));
    let key = legend_style(&l);
    let states = LEGEND.into_iter().filter(|&s| map.has(s));
    for (state, row, at) in legend_places(&l, p.fonts, px(r.w), states) {
        if row >= LEGEND_ROWS {
            break;
        }
        let y = legend_y + row * l.legend_pitch;
        let (color, edge) = look(state, ring);
        let edge = if state == CellState::Pending { Some((LINE, 1)) } else { edge };
        let x = px(r.x) + at;
        cell(p, Rect::new(whole_px(x), y + (line_h - side) / 2, side, side), color, edge);
        let base = p.mid(l.text.label, y, line_h);
        p.text(x + px(side) + l.m(6.0), base, legend_name(state), key);
    }
}

fn legend_style(l: &Layout) -> Style {
    Pen::style(Face::Regular, l.text.label, TEXT)
}

/// Each state's row of the legend and its left edge in pixels from the widget's, the states
/// flowing left to right and wrapping at `width`. A state that does not fit in `LEGEND_ROWS` rows
/// gets a row past them.
fn legend_places(
    l: &Layout,
    fonts: &mut Fonts,
    width: f32,
    states: impl Iterator<Item = CellState>,
) -> heapless::Vec<(CellState, u32, f32), { LEGEND.len() }> {
    let key = legend_style(l);
    let (mut x, mut row) = (0.0, 0);
    let mut places = heapless::Vec::new();
    for state in states {
        let entry = px(whole_px(l.m(12.0))) + l.m(6.0) + fonts.measure(legend_name(state), key);
        if x > 0.0 && x + entry > width {
            (x, row) = (0.0, row + 1);
        }
        // At most `LEGEND.len()` states are listed, which is its capacity.
        let _ = places.push((state, row, x));
        x += entry + l.m(14.0);
    }
    places
}

/// A fault's 64-bit pattern in hex, the digits that differ from `reference` inverted.
fn hex_diff(p: &mut Pen<'_>, x: f32, top: u32, value: u64, reference: u64) {
    let l = *p.l;
    let style = p.body(TEXT);
    let text = fixed::<16>(format_args!("{value:016X}"));
    let expected = fixed::<16>(format_args!("{reference:016X}"));
    let base = p.mid(l.text.base, top, l.row_h);
    let mut at = x;
    for (c, e) in text.chars().zip(expected.chars()) {
        let mut buf = [0; 4];
        let glyph = c.encode_utf8(&mut buf);
        let w = p.measure(glyph, style);
        if c == e {
            p.text(at, base, glyph, style);
        } else {
            p.canvas.fill(Rect::new(whole_px(at), top, whole_px(w), l.row_h), ERR);
            p.text(at, base, glyph, Style { face: Face::Bold, color: BG, ..style });
        }
        at += w;
    }
}

pub fn errors(p: &mut Pen<'_>, r: Rect, f: &Frame<'_>) {
    let s = f.status();
    let l = *p.l;
    let color = if s.faults > 0 { ERR } else { PASS };
    let big = Pen::style(Face::Bold, l.text.big, color);
    let base = px(r.y) + f32::from(l.text.big) * 0.875;
    let mut x = px(r.x);
    x += p.text(x, base, &num(s.faults), big) + l.m(14.0);
    let word = match s.faults {
        0 => "NO ERRORS",
        1 => "ERROR",
        _ => "ERRORS",
    };
    let label = Style { px: l.text.base, ..p.label_style(color) };
    p.text(x, base, word, label);

    let body = Rect::new(
        r.x,
        r.y + u32::from(l.text.big) + whole_px(l.m(12.0)),
        r.w,
        r.h.saturating_sub(u32::from(l.text.big) + whole_px(l.m(12.0))),
    );
    let last: Option<&FaultRecord> = f.snapshot.faults.back().filter(|_| s.faults > 0);
    let Some(last) = last else {
        let analysis = fixed::<64>(format_args!("{}", s.analysis));
        p.line(body, 0, "Analysis", &analysis, TEXT);
        p.line(body, 1, "Last error", NONE, DIM);
        let passes = match s.passes {
            0 => fixed::<32>(format_args!("first pass running")),
            n => fixed(format_args!("{n} complete")),
        };
        p.line(body, 2, "Passes", &passes, TEXT);
        return;
    };
    let fault = last.fault;
    let key_w = l.m(100.0);
    let mut row = 0;
    let (address, expected) = (addr(fault.addr.get() as u64), fixed::<16>(format_args!("{:016X}", fault.expected)));
    if let Some(top) = slot(&l, body, &mut row) {
        p.line(body, top, "Address", &address, STRONG);
    }
    if let Some(top) = slot(&l, body, &mut row) {
        p.line(body, top, "Expected", &expected, TEXT);
    }
    if let Some(top) = slot(&l, body, &mut row) {
        let base = p.mid(l.text.base, body.y + top * l.pitch, l.row_h);
        p.fit(px(r.x), base, "Read", p.body(DIM), key_w);
        hex_diff(p, px(r.x) + key_w, body.y + top * l.pitch, fault.actual, fault.expected);
    }
    if let Some(top) = slot(&l, body, &mut row) {
        let analysis = fixed::<64>(format_args!("{}", s.analysis));
        p.line(body, top, "Analysis", &analysis, STRONG);
    }
    if let Some(top) = slot(&l, body, &mut row) {
        let origin =
            fixed::<96>(format_args!("{} {SEPARATOR} cpu {} {SEPARATOR} pass {}", last.test, last.cpu, last.pass + 1));
        p.line(body, top, "Found in", &origin, TEXT);
    }
    for rec in f.snapshot.faults.iter().rev().skip(1) {
        let Some(top) = slot(&l, body, &mut row) else { break };
        let flipped = rec.fault.actual ^ rec.fault.expected;
        let line = fixed::<96>(format_args!(
            "{} {SEPARATOR} flipped {flipped:016X} {SEPARATOR} cpu {}",
            addr(rec.fault.addr.get() as u64),
            rec.cpu
        ));
        let base = p.mid(l.text.base, body.y + top * l.pitch, l.row_h);
        p.fit(px(r.x), base, &line, p.body(DIM), px(r.w));
    }
}

/// The next row of `body` when there is room for it.
fn slot(l: &Layout, body: Rect, row: &mut u32) -> Option<u32> {
    let this = *row;
    (body.y + (this + 1) * l.pitch <= body.bottom()).then(|| {
        *row += 1;
        this
    })
}

/// A medium's name with a capital, for headings.
struct Titled(Medium);

impl core::fmt::Display for Titled {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self.0 {
            Medium::Ethernet => "Ethernet",
            Medium::WiFi => "Wi-Fi",
        })
    }
}

pub fn net(p: &mut Pen<'_>, r: Rect, f: &Frame<'_>) {
    let Some(net) = f.net else {
        p.line(r, 0, "Status", "no network", ERR);
        if let Some(why) = f.net_err {
            p.line(r, 1, "Wired", why.wired, ERR);
            p.line(r, 2, "Wi-Fi", why.wifi, ERR);
        }
        let l = *p.l;
        let base = p.mid(l.text.small, r.y + 3 * l.pitch, l.row_h);
        p.fit(
            px(r.x),
            base,
            "needs a wired NIC or Wi-Fi with an address",
            Pen::style(Face::Regular, l.text.small, DIM),
            px(r.w),
        );
        return;
    };
    match net.ports() {
        [port] => single_port(p, r, port),
        ports => port_list(p, r, ports, net.omitted),
    }
}

/// The one port in full: link, address, the page to open, and the rest.
fn single_port(p: &mut Pen<'_>, r: Rect, port: &PortStatus) {
    let l = *p.l;
    let link = port.link;
    let (state, color) = if link.up { ("up", PASS) } else { ("down", ERR) };
    let head = fixed::<32>(format_args!("{} {state}", Titled(link.medium)));
    let end = p.line(r, 0, "Link", &head, color);
    let base = p.mid(l.text.base, r.y, l.row_h);
    p.fit(
        end + l.m(12.0),
        base,
        &fixed::<24>(format_args!("{}", link.mac)),
        p.body(DIM),
        px(r.right()) - end - l.m(12.0),
    );
    if let Some(cidr) = link.address {
        p.line(r, 1, "Address", &num(cidr.address()), STRONG);
        p.line(r, 2, "Open", &fixed::<32>(format_args!("http://{}/", cidr.address())), LABEL);
    } else {
        p.line(r, 1, "Address", if link.up { "waiting for DHCP" } else { "none" }, TEXT);
    }
    let tail = fixed::<128>(format_args!(
        "{} {SEPARATOR} gw {} {SEPARATOR} dns {}",
        ferrite_net::mdns::HOST,
        ferrite_net::OptAddr(link.gateway),
        ferrite_net::OptAddr(link.dns)
    ));
    p.line(r, 3, "Host", &tail, DIM);
}

/// Several ports, a row each.
fn port_list(p: &mut Pen<'_>, r: Rect, ports: &[PortStatus], omitted: usize) {
    let l = *p.l;
    let rows = (r.h / l.pitch.max(1)) as usize;
    let shown = if ports.len() > rows { rows.saturating_sub(1).max(1) } else { ports.len() };
    for (n, port) in ports.iter().take(shown).enumerate() {
        let link = port.link;
        let (text, color) = match link.address {
            Some(cidr) if link.up => (fixed::<48>(format_args!("{} {SEPARATOR} up", cidr.address())), PASS),
            None if link.up => (fixed(format_args!("waiting for DHCP")), TEXT),
            _ => (fixed(format_args!("link down")), ERR),
        };
        p.line(r, u32::try_from(n).unwrap_or(0), &fixed::<24>(format_args!("{}", Titled(link.medium))), &text, color);
    }
    if omitted > 0 || shown < ports.len() {
        let more = fixed::<32>(format_args!("+{} more", ports.len() - shown + omitted));
        let base = p.mid(l.text.small, r.y + u32::try_from(shown).unwrap_or(0) * l.pitch, l.row_h);
        p.text(px(r.x), base, &more, Pen::style(Face::Regular, l.text.small, DIM));
    }
}

pub fn log(p: &mut Pen<'_>, r: Rect, f: &Frame<'_>) {
    let s = f.status();
    let l = *p.l;
    let base = p.mid(l.text.label, r.y, r.h);
    let style = Pen::style(Face::Regular, l.text.label, DIM);
    let mut x = px(r.x) + l.m(20.0);
    let history = f.snapshot.history;
    let last_fault = f.snapshot.faults.back().filter(|_| s.faults > 0);
    if let Some(notice) = p.notice {
        let ink = if notice.failed { WARN } else { PASS };
        x += p.fit(x, base, &notice.text, Style { color: ink, ..style }, px(r.right()) / 2.0 - x) + l.m(14.0);
    } else if let Some(rec) = last_fault {
        let text = fixed::<128>(format_args!(
            "ERROR {} {SEPARATOR} {} {SEPARATOR} cpu {}",
            rec.test,
            addr(rec.fault.addr.get() as u64),
            rec.cpu
        ));
        x += p.fit(x, base, &text, Style { color: ERR, ..style }, px(r.right()) / 2.0 - x) + l.m(14.0);
    } else if let Some(rec) = history.tests.back() {
        let text = fixed::<128>(format_args!(
            "pass {} {SEPARATOR} {} finished in {}",
            rec.pass + 1,
            rec.test,
            format::took(rec.took)
        ));
        x += p.fit(x, base, &text, style, px(r.right()) / 2.0 - x) + l.m(14.0);
    }
    let (i, n) = s.position;
    let goal: String<64> = if s.outlook.goal.is_unlimited() {
        String::new()
    } else {
        fixed(format_args!(" {SEPARATOR} goal: {}", s.outlook.goal))
    };
    let line: String<192> = if n == 0 {
        fixed(format_args!("[{}] preparing the buffer", hms(f.elapsed())))
    } else {
        fixed(format_args!(
            "[{}] pass {} {SEPARATOR} test {i}/{n} {SEPARATOR} {}{goal}",
            hms(f.elapsed()),
            s.passes + 1,
            s.test
        ))
    };
    p.fit(x, base, &line, Style { color: STRONG, ..style }, px(r.right()) - x);
}

pub fn hints(p: &mut Pen<'_>, r: Rect, f: &Frame<'_>) {
    let s = f.status();
    let l = *p.l;
    p.canvas.fill(Rect::new(r.x, r.y, r.w, 1), BAR_RULE);
    let pause = if s.paused { "Resume" } else { "Pause" };
    let left = [("F1", "Help", false)];
    let right = [
        ("P", pause, true),
        ("R", "Restart", true),
        ("S", "Skip test", false),
        ("\u{2190} \u{2192}", "Tabs", false),
        ("F10", "Save report", true),
        ("Q", "Quit", true),
    ];
    let (gap, pad, inner) = (l.m(20.0), l.m(6.0), l.m(7.0));
    let cap_h = whole_px(l.m(19.0)).min(r.h.saturating_sub(1));
    let top = r.y + 1 + (r.h.saturating_sub(1 + cap_h)) / 2;
    let base = p.mid(l.text.label, top, cap_h);
    let text_style = |on: bool| Pen::style(Face::Regular, l.text.label, if on { BAR_TEXT } else { BAR_DIM });
    let cap_style = |on: bool| Pen::style(Face::Bold, l.text.label, if on { KEY_INK } else { BAR_DIM });
    let width = |p: &mut Pen<'_>, items: &[(&str, &str, bool)]| -> f32 {
        items
            .iter()
            .map(|&(key, what, on)| p.measure(key, cap_style(on)) + 2.0 * pad + inner + p.measure(what, text_style(on)))
            .sum::<f32>()
            + gap * px(u32::try_from(items.len().saturating_sub(1)).unwrap_or(0))
    };
    let total = width(p, &right);
    let draw = |p: &mut Pen<'_>, mut x: f32, items: &[(&str, &str, bool)]| {
        for &(key, what, on) in items {
            let cap_w = p.measure(key, cap_style(on)) + 2.0 * pad;
            p.canvas.fill(Rect::new(whole_px(x), top, whole_px(cap_w), cap_h), if on { KEY } else { BAR_RULE });
            p.text(x + pad, base, key, cap_style(on));
            x += cap_w + inner;
            x += p.text(x, base, what, text_style(on)) + gap;
        }
    };
    draw(p, px(r.x) + l.m(20.0), &left);
    draw(p, px(r.right()) - l.m(20.0) - total, &right);
}

#[cfg(test)]
mod tests {
    use std::format;
    use std::string::ToString as _;

    use assert2::check;
    use rstest::rstest;

    use super::*;

    /// Every size the layout is checked at in `layout.rs`, and the wide screens beyond.
    #[rstest]
    #[case::smallest(768, 576)]
    #[case::compact(1024, 768)]
    #[case::ovmf_default(1280, 800)]
    #[case::laptop(1366, 768)]
    #[case::full_hd(1920, 1080)]
    #[case::uhd(3840, 2160)]
    fn the_title_bars_figures_fit_beside_each_other_at_their_longest(#[case] w: u32, #[case] h: u32) {
        let l = Layout::new(w, h);
        let mut fonts = Fonts::load().expect("fonts");
        let plain = clock_style(&l);
        let loud = Style { face: Face::Bold, ..plain };
        // The widest state, the run clock of a 365 day run, and the widest time of day.
        let longest = [
            (format!("99999 ERRORS {SEPARATOR} PAUSED"), loud),
            ("ELAPSED 8760:00:00".to_string(), plain),
            ("12:59:59 PM".to_string(), plain),
        ];
        let used: f32 =
            longest.iter().map(|(text, style)| fonts.measure(text, *style)).sum::<f32>() + 2.0 * l.m(CLOCK_GAP);
        check!(used <= px(l.clock.w), "{used} of {} px at {w}x{h}", l.clock.w);
    }

    #[rstest]
    #[case::smallest(768, 576)]
    #[case::compact(1024, 768)]
    #[case::ovmf_default(1280, 800)]
    #[case::laptop(1366, 768)]
    #[case::full_hd(1920, 1080)]
    #[case::uhd(3840, 2160)]
    fn every_state_of_the_memory_maps_legend_fits_in_its_rows(#[case] w: u32, #[case] h: u32) {
        let l = Layout::new(w, h);
        let mut fonts = Fonts::load().expect("fonts");
        let places = legend_places(&l, &mut fonts, px(l.map.body.w), LEGEND.into_iter());
        let rows = places.iter().map(|&(_, row, _)| row).max().unwrap_or(0) + 1;
        check!(rows <= LEGEND_ROWS, "{rows} rows at {w}x{h}");
    }
}
