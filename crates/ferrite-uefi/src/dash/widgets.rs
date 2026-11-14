//! The static chrome behind the widgets, and each widget's drawing. A widget draws only inside its
//! own rectangle, starting from a cleared ground, so it can be redrawn alone.

#![expect(
    clippy::many_single_char_names,
    reason = "p, r, f, s and l are the pen, rectangle, frame, status and layout in every widget"
)]

use ferrite_mem::suite::{Mode, TestKind};
use ferrite_net::{Medium, PortStatus};
use heapless::String;

use super::format::{self, addr, fixed, hms, num, plural};
use super::layout::{Layout, MAP_COLS, MAP_ROWS, Section, window};
use super::theme::{
    ACTIVE, ACTIVE_RING, BAR, BAR_DIM, BAR_RULE, BAR_TEXT, BG, DIM, ERR, FILL, HELD, KEY, KEY_INK, LABEL, LINE, PASS,
    PENDING, SEL, SEL_INK, STRONG, TAB_ON, TAB_ON_INK, TAB_TEXT, TESTED, TEXT, TRACK,
};
use super::{Frame, State};
use crate::gfx::text::{Case, Face, Fonts, Style};
use crate::gfx::{Canvas, Rect, Rgb, px, whole_px};
use crate::report::{CpuInfo, FaultRecord, TestRecord};

/// The placeholder for what this build cannot read from the machine.
const NONE: &str = "\u{2014}";
const SEPARATOR: &str = "\u{b7}";

/// Cells of the memory map.
pub const CELLS: usize = (MAP_COLS * MAP_ROWS) as usize;

/// The memory map cell a test at `percent` is working in.
#[must_use]
pub fn active_cell(percent: u16) -> usize {
    (usize::from(percent.min(100)) * CELLS / 100).min(CELLS - 1)
}

pub struct Pen<'d> {
    pub canvas: &'d mut Canvas,
    pub fonts: &'d mut Fonts,
    pub l: &'d Layout,
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
    fn bar(&mut self, r: Rect, permille: u32, fill: Rgb) {
        let inner = Rect::new(r.x + 1, r.y + 1, r.w.saturating_sub(2), r.h.saturating_sub(2));
        self.canvas.fill(r, LINE);
        self.canvas.fill(inner, TRACK);
        let filled = inner.w * permille.min(1000) / 1000;
        self.canvas.fill(Rect::new(inner.x, inner.y, filled, inner.h), fill);
    }

    /// One line: a label column, a bar and a right-aligned figure.
    fn bar_row(&mut self, row: Rect, label: &str, figure: &str, permille: u32, fill: Rgb) {
        let l = *self.l;
        let gap = whole_px(l.m(12.0));
        let base = self.mid(l.text.base, row.y, row.h);
        self.fit(px(row.x), base, label, self.body(DIM), px(l.lab));
        let bar_h = whole_px(l.m(16.0)).min(row.h);
        let x = row.x + l.lab + gap;
        let w = row.w.saturating_sub(l.lab + l.pw + 2 * gap);
        self.bar(Rect::new(x, row.y + (row.h - bar_h) / 2, w, bar_h), permille, fill);
        self.right(px(row.right()), base, figure, self.body(TEXT));
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
        let w = self.measure("ERRORS", style);
        self.text(cols.errors_r - w, base, "ERRORS", style);
        let w = self.measure("TIME", style);
        self.text(cols.time_r - w, base, "TIME", style);
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
    time_r: f32,
}

impl TestCols {
    fn of(l: &Layout, r: Rect) -> Self {
        let [num_w, status_w, errors_w, time_w] = l.cols.map(px);
        let (pad, gap) = (l.m(14.0), l.m(8.0));
        let num_x = px(r.x) + pad;
        let name_x = num_x + num_w + gap;
        let time_r = px(r.right()) - pad;
        let errors_r = time_r - time_w - gap;
        let status_x = errors_r - errors_w - gap - status_w;
        Self { num_x, name_x, name_max: status_x - gap - name_x, status_x, errors_r, time_r }
    }
}

pub fn clock(p: &mut Pen<'_>, r: Rect, f: &Frame<'_>) {
    let s = f.status();
    let l = *p.l;
    let base = p.mid(l.text.label, r.y, r.h);
    let plain = Style { tracking: l.m(0.5), ..Pen::style(Face::Regular, l.text.label, BAR_DIM) };
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
    right -= p.right(right, base, &state, state_style) + l.m(22.0);
    p.right(
        right,
        base,
        &fixed::<32>(format_args!("ELAPSED {}", hms(f.elapsed()))),
        Style { color: BAR_TEXT, ..plain },
    );
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
    let cpu: String<48> = if list.is_empty() {
        fixed(format_args!("1 logical"))
    } else {
        let first_of =
            |i: usize, same: fn(&CpuInfo, &CpuInfo) -> bool| !list[..i].iter().any(|before| same(before, &list[i]));
        let cores = (0..list.len()).filter(|&i| first_of(i, |a, b| (a.package, a.core) == (b.package, b.core))).count();
        fixed(format_args!("{} logical {SEPARATOR} {cores} core{}", list.len(), plural(cores)))
    };
    let memory: String<48> = if s.coverage.free > 0 {
        let (tested, free) =
            (usize::try_from(s.coverage.tested).unwrap_or(0), usize::try_from(s.coverage.free).unwrap_or(0));
        fixed(format_args!("{} of {} free ({}%)", format::size(tested), format::size(free), s.coverage.percent()))
    } else {
        fixed(format_args!("{} under test", format::size(s.bytes)))
    };
    let preset: &str = s.preset.into();
    p.pairs(
        r,
        &[
            ("CPU", &cpu),
            ("Memory", &memory),
            ("Preset", preset),
            ("Tier", s.tier),
            ("Modules", NONE),
            ("Timings", NONE),
        ],
    );
}

fn permille(percent: u16) -> u32 {
    u32::from(percent.min(100)) * 10
}

fn fill_of(paused: bool) -> Rgb {
    if paused { HELD } else { FILL }
}

pub fn pass(p: &mut Pen<'_>, r: Rect, f: &Frame<'_>) {
    let s = f.status();
    let l = *p.l;
    let gap = l.m(8.0);
    let overall = Rect::new(r.x, r.y, r.w, l.row_h);
    p.bar_row(overall, "Overall", NONE, 0, fill_of(s.paused));
    let pass = Rect::new(r.x, r.y + l.row_h + whole_px(gap), r.w, l.row_h);
    let figure = fixed::<32>(format_args!("{}% {SEPARATOR} {}", s.percent.min(100), format::clock(f.pass_elapsed())));
    let label = fixed::<24>(format_args!("Pass {}", s.passes + 1));
    p.bar_row(pass, &label, &figure, permille(s.percent), fill_of(s.paused));
}

pub fn test(p: &mut Pen<'_>, r: Rect, f: &Frame<'_>) {
    let s = f.status();
    let (i, n) = s.position;
    if n == 0 {
        p.bar_row(r, "Test", NONE, 0, fill_of(s.paused));
        return;
    }
    let label = fixed::<24>(format_args!("Test {i}/{n}"));
    let figure = fixed::<8>(format_args!("{}%", s.test_percent.min(100)));
    p.bar_row(r, &label, &figure, permille(s.test_percent), fill_of(s.paused));
}

pub fn detail(p: &mut Pen<'_>, r: Rect, f: &Frame<'_>) {
    let s = f.status();
    let total = f.snapshot.history.cpus.len().max(s.cpus).max(1);
    let threads: String<48> = match s.mode {
        Mode::Split => fixed(format_args!("{} / {total} parallel", s.cpus.min(total))),
        Mode::Single => fixed(format_args!("1 / {total} {SEPARATOR} single core")),
    };
    let rate = f
        .snapshot
        .history
        .tests
        .back()
        .and_then(TestRecord::rate)
        .map_or_else(|| fixed(format_args!("{NONE}")), format::rate);
    p.pairs(r, &[("Range", NONE), ("Threads", &threads), ("Pattern", NONE), ("Last rate", &rate)]);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Row {
    Done,
    Current,
    Pending,
}

pub fn tests(p: &mut Pen<'_>, r: Rect, f: &Frame<'_>) {
    let s = f.status();
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
        if row == Row::Current {
            p.canvas.fill(Rect::new(r.x, top, r.w, l.pitch), SEL);
        }
        let selected = row == Row::Current;
        let ink = |color: Rgb| if selected { SEL_INK } else { color };
        let base = p.mid(l.text.base, top, l.pitch);
        p.text(cols.num_x, base, &num(i + 1), p.body(ink(DIM)));
        let name_color = if row == Row::Pending { TEXT } else { STRONG };
        p.fit(cols.name_x, base, kind.name(), p.body(ink(name_color)), cols.name_max);

        let (status, status_color) = match (row, record) {
            (Row::Current, _) if s.paused => ("PAUSED", SEL_INK),
            (Row::Current, _) => ("RUNNING", SEL_INK),
            (Row::Pending, _) => ("QUEUED", DIM),
            (Row::Done, Some(rec)) if rec.faults > 0 => ("FAIL", ERR),
            (Row::Done, Some(_)) => ("PASS", PASS),
            (Row::Done, None) => ("DONE", DIM),
        };
        let status_style = Pen::style(Face::SemiBold, l.text.base, status_color);
        p.text(cols.status_x, base, status, status_style);

        match record {
            Some(rec) if rec.faults > 0 => {
                p.right(cols.errors_r, base, &num(rec.faults), p.body(ERR));
            },
            Some(_) => {
                p.right(cols.errors_r, base, "0", p.body(TEXT));
            },
            None => {
                p.right(cols.errors_r, base, NONE, p.body(ink(TEXT)));
            },
        }
        let took = record.map_or_else(|| fixed(format_args!("{NONE}")), |rec| format::took(rec.took));
        p.right(cols.time_r, base, &took, p.body(ink(DIM)));
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

pub fn map(p: &mut Pen<'_>, r: Rect, f: &Frame<'_>) {
    let s = f.status();
    let l = *p.l;
    let small = Pen::style(Face::Regular, l.text.small, DIM);
    let base = p.mid(l.text.small, r.y, l.row_h);
    p.text(px(r.x), base, "0", small);
    p.right(px(r.right()), base, &format::size(s.bytes), small);
    let each = fixed::<32>(format_args!("1 cell = {}", format::size(s.bytes / CELLS)));
    let w = p.measure(&each, small);
    p.text(px(r.x) + (px(r.w) - w) / 2.0, base, &each, small);

    let (gap, cell_h) = (whole_px(l.m(3.0)), whole_px(l.m(20.0)));
    let top = r.y + l.row_h + whole_px(l.m(6.0));
    let active = (s.position.1 > 0).then(|| active_cell(s.test_percent));
    let ring = whole_px(l.m(2.0)).max(1);
    for i in 0..CELLS {
        let (col, row) =
            (u32::try_from(i % MAP_COLS as usize).unwrap_or(0), u32::try_from(i / MAP_COLS as usize).unwrap_or(0));
        let x0 = r.x + col * (r.w + gap) / MAP_COLS;
        let x1 = r.x + (col + 1) * (r.w + gap) / MAP_COLS - gap;
        let rect = Rect::new(x0, top + row * (cell_h + gap), x1 - x0, cell_h);
        match active {
            Some(a) if i == a => cell(p, rect, ACTIVE, Some((ACTIVE_RING, ring))),
            Some(a) if i < a => cell(p, rect, TESTED, None),
            _ => cell(p, rect, PENDING, None),
        }
    }

    let legend = Rect::new(r.x, top + MAP_ROWS * (cell_h + gap) - gap + whole_px(l.m(10.0)), r.w, whole_px(l.m(16.0)));
    let key = Pen::style(Face::Regular, l.text.label, TEXT);
    let side = whole_px(l.m(12.0));
    let mut x = px(r.x);
    let swatches = [
        ("Tested", TESTED, None),
        ("Active", ACTIVE, Some((ACTIVE_RING, ring))),
        ("Pending", PENDING, Some((LINE, 1))),
    ];
    for (label, color, edge) in swatches {
        let box_ = Rect::new(whole_px(x), legend.y + (legend.h - side) / 2, side, side);
        cell(p, box_, color, edge);
        let base = p.mid(l.text.label, legend.y, legend.h);
        x += px(side) + l.m(6.0);
        x += p.text(x, base, label, key) + l.m(14.0);
    }
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
    if let Some(rec) = last_fault {
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
    let line: String<128> = if n == 0 {
        fixed(format_args!("[{}] preparing the buffer", hms(f.elapsed())))
    } else {
        fixed(format_args!(
            "[{}] pass {} {SEPARATOR} test {i}/{n} {SEPARATOR} {}",
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
        ("F10", "Save report", false),
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
