//! The cards behind the widgets, and each widget's drawing. A widget draws only inside its own
//! rectangle, starting from a cleared ground, so it can be redrawn alone.

#![expect(
    clippy::many_single_char_names,
    reason = "p, r, f, s and l are the pen, rectangle, frame, status and layout in every widget"
)]

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use ferrite_mem::suite::{Mode, TestKind};

use super::format;
use super::layout::{Layout, px, tile_grid, window};
use super::theme::{
    ACCENT, ACCENT_FROM, BAD, BAD_DEEP, BAD_EDGE, BAD_GROUND, CARD, CARD_EDGE, CHIP, DIM, GOOD, MUTED, PAGE, ROW, TEXT,
    TRACK, WARN,
};
use super::{Frame, State};
use crate::gfx::text::{Face, Fonts, Style};
use crate::gfx::{Canvas, Fill, Rect, Rgb};
use crate::report::TestRecord;

/// The name the mDNS responder answers for ([`crate::mdns::HOST`]).
const MDNS_NAME: &str = "ferrite.local";

pub struct Pen<'d> {
    pub canvas: &'d mut Canvas,
    pub fonts: &'d mut Fonts,
    pub l: &'d Layout,
    /// Faults were found: the header and the fault log turn red.
    pub faulted: bool,
}

impl Pen<'_> {
    fn header_ground(&self) -> Rgb {
        if self.faulted { BAD_GROUND } else { CARD }
    }

    /// Clear `r` to what lies behind it and clip drawing to it.
    pub fn begin(&mut self, r: Rect) {
        let ground = if self.l.header.intersect(r) == r {
            self.header_ground()
        } else if self.l.footer.intersect(r) == r {
            PAGE
        } else {
            CARD
        };
        self.canvas.set_clip(r);
        self.canvas.fill(r, ground);
    }

    /// The page and the cards, with their titles.
    pub fn backdrop(&mut self) {
        let l = *self.l;
        let all = self.canvas.bounds();
        self.canvas.set_clip(all);
        self.canvas.fill(all, PAGE);
        let (header_edge, fault_edge) = if self.faulted { (BAD_EDGE, BAD_EDGE) } else { (CARD_EDGE, CARD_EDGE) };
        self.card(l.header, self.header_ground(), header_edge);
        self.card(l.progress, CARD, CARD_EDGE);
        let titled = [
            (l.tests_card, "Tests in this pass", CARD_EDGE, MUTED),
            (l.faults_card, "Faults", fault_edge, if self.faulted { BAD } else { MUTED }),
            (l.net_card, "Network", CARD_EDGE, MUTED),
            (l.passes_card, "Passes", CARD_EDGE, MUTED),
            (l.cpus_card, "Processors", CARD_EDGE, MUTED),
        ];
        for (card, title, edge, color) in titled {
            self.card(card, CARD, edge);
            let base = self.mid(l.text.label, card.y + l.pad, l.title_h);
            self.label(px(card.x + l.pad), base, title, color);
        }
    }

    fn card(&mut self, r: Rect, fill: Rgb, edge: Rgb) {
        let (x, y, w, h) = (px(r.x), px(r.y), px(r.w), px(r.h));
        let radius = self.l.radius;
        self.canvas.round_rect(x, y, w, h, radius, Fill::Solid(edge));
        self.canvas.round_rect(x + 1.0, y + 1.0, w - 2.0, h - 2.0, radius - 1.0, Fill::Solid(fill));
    }

    const fn style(face: Face, px: u16, color: Rgb) -> Style {
        Style { face, px, color, tracking: 0.0 }
    }

    fn label_style(&self, color: Rgb) -> Style {
        let px = self.l.text.label;
        Style { face: Face::SemiBold, px, color, tracking: f32::from(px) * 0.08 }
    }

    /// Small capitals: `text` is uppercased here.
    fn label(&mut self, x: f32, base: f32, text: &str, color: Rgb) -> f32 {
        let style = self.label_style(color);
        self.canvas.text(self.fonts, x, base, &text.to_ascii_uppercase(), style)
    }

    fn label_width(&mut self, text: &str) -> f32 {
        let style = self.label_style(MUTED);
        self.fonts.measure(&text.to_ascii_uppercase(), style)
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

    /// A progress bar of `permille` thousandths, rounded at both ends.
    fn bar(&mut self, x: f32, y: f32, w: f32, h: f32, permille: u32, fill: Fill) {
        self.canvas.round_rect(x, y, w, h, h / 2.0, Fill::Solid(TRACK));
        if permille > 0 {
            let filled = (w * px(permille.min(1000)) / 1000.0).max(h);
            self.canvas.round_rect(x, y, filled, h, h / 2.0, fill);
        }
    }

    fn pill_width(&mut self, text: &str, style: Style, dot: bool) -> f32 {
        let pad = self.l.m(9.0);
        let dot_w = if dot { self.l.m(12.0) } else { 0.0 };
        self.measure(text, style) + 2.0 * pad + dot_w
    }

    /// A rounded chip from `x`, centred in the band `r`; returns its width.
    fn pill(&mut self, x: f32, r: Rect, text: &str, style: Style, ground: Rgb, dot: Option<Rgb>) -> f32 {
        let w = self.pill_width(text, style, dot.is_some());
        let h = self.l.m(24.0).min(px(r.h));
        let y = px(r.y) + (px(r.h) - h) / 2.0;
        self.canvas.round_rect(x, y, w, h, h / 2.0, Fill::Solid(ground));
        let mut tx = x + self.l.m(9.0);
        if let Some(dot) = dot {
            self.canvas.circle(tx + self.l.m(3.5), y + h / 2.0, self.l.m(3.5), dot);
            tx += self.l.m(12.0);
        }
        let base = self.fonts.centred_baseline(style.px, y, h);
        self.text(tx, base, text, style);
        w
    }
}

pub fn brand(p: &mut Pen<'_>, r: Rect, f: &Frame<'_>) {
    let s = f.status();
    let l = *p.l;
    let word = Pen::style(Face::Bold, l.text.title + l.text.title / 3, TEXT);
    let base = p.mid(word.px, r.y, r.h);
    let mut x = px(r.x);
    x += p.text(x, base, "ferrite", word);
    x += l.m(8.0);
    x += p.text(x, base, "memory test", Pen::style(Face::Regular, l.text.body, MUTED));
    x += l.m(18.0);
    let preset: &str = s.preset.into();
    let cpus = f.snapshot.history.cpus.len().max(1);
    let chips = [
        String::from(preset),
        String::from(s.tier),
        format::size(s.bytes),
        format!("{cpus} cpu{}", if cpus == 1 { "" } else { "s" }),
    ];
    let style = Pen::style(Face::Regular, l.text.small, MUTED);
    for chip in &chips {
        if x + p.pill_width(chip, style, false) > px(r.right()) {
            break;
        }
        x += p.pill(x, r, chip, style, CHIP, None) + l.m(6.0);
    }
}

pub fn clock(p: &mut Pen<'_>, r: Rect, f: &Frame<'_>) {
    let s = f.status();
    let l = *p.l;
    let time = Pen::style(Face::Mono, l.text.title + l.text.title / 6, TEXT);
    let base = p.mid(time.px, r.y, r.h);
    let mut right = px(r.right());
    right -= p.right(right, base, &format::clock(f.elapsed()), time) + l.m(8.0);
    right -= p.label_width("elapsed");
    p.label(right, base, "elapsed", DIM);
    right -= l.m(18.0);

    let (text, color) = match f.state() {
        State::Starting => ("Starting", MUTED),
        State::Running => ("Running", GOOD),
        State::Paused => ("Paused", WARN),
    };
    let style = Pen::style(Face::SemiBold, l.text.small, color);
    right -= p.pill_width(text, style, true);
    p.pill(right, r, text, style, CHIP, Some(color));
    if s.faults > 0 {
        let text = format!("{} fault{}", s.faults, if s.faults == 1 { "" } else { "s" });
        let style = Pen::style(Face::Bold, l.text.small, TEXT);
        right -= p.pill_width(&text, style, true) + l.m(8.0);
        p.pill(right, r, &text, style, BAD_DEEP, Some(TEXT));
    }
}

/// The text band and the bar below it in a progress widget.
fn split_bar(r: Rect, bar_h: f32, gap: f32) -> (Rect, f32) {
    let text_h = (px(r.h) - bar_h - gap).max(0.0);
    let band = Rect::new(r.x, r.y, r.w, super::layout::whole_px(text_h));
    (band, px(r.bottom()) - bar_h)
}

fn percent_permille(percent: u16) -> u32 {
    u32::from(percent.min(100)) * 10
}

pub fn pass(p: &mut Pen<'_>, r: Rect, f: &Frame<'_>) {
    let s = f.status();
    let l = *p.l;
    let bar_h = l.m(10.0);
    let (band, bar_y) = split_bar(r, bar_h, l.m(7.0));
    let title = Pen::style(Face::SemiBold, l.text.title, TEXT);
    let base = p.mid(title.px, band.y, band.h);
    let mut x = px(r.x);
    x += p.text(x, base, &format!("Pass {}", s.passes + 1), title) + l.m(10.0);
    let done = match s.passes {
        0 => String::from("first pass"),
        1 => String::from("1 complete"),
        n => format!("{n} complete"),
    };
    let small = Pen::style(Face::Regular, l.text.small, MUTED);
    p.text(x, base, &done, small);

    let mut right = px(r.right());
    let percent = Pen::style(Face::MonoBold, l.text.title, TEXT);
    right -= p.right(right, base, &format!("{}%", s.percent.min(100)), percent) + l.m(14.0);
    let took = f.pass_elapsed();
    let mut times = format::clock(took);
    if s.percent >= 2 && s.percent < 100 && !s.paused {
        let left = took * u32::from(100 - s.percent) / u32::from(s.percent);
        times = format!("{times} elapsed \u{b7} about {} left", format::clock(left));
    } else if s.position.1 > 0 {
        times.push_str(" elapsed");
    }
    p.right(right, base, &times, small);

    let fill = if s.paused { Fill::Solid(WARN) } else { Fill::Across(ACCENT_FROM, ACCENT) };
    p.bar(px(r.x), bar_y, px(r.w), bar_h, percent_permille(s.percent), fill);
}

pub fn test(p: &mut Pen<'_>, r: Rect, f: &Frame<'_>) {
    let s = f.status();
    let l = *p.l;
    let bar_h = l.m(6.0);
    let (band, bar_y) = split_bar(r, bar_h, l.m(8.0));
    let base = p.mid(l.text.body, band.y, band.h);
    let (i, n) = s.position;
    let mut x = px(r.x);
    let mut right = px(r.right());
    let small = Pen::style(Face::Regular, l.text.small, MUTED);
    if n == 0 {
        p.label(x, base, "test", MUTED);
        p.text(x + l.m(60.0), base, "preparing the buffer", small);
        p.bar(px(r.x), bar_y, px(r.w), bar_h, 0, Fill::Solid(ACCENT));
        return;
    }
    x += p.label(x, base, &format!("test {i}/{n}"), MUTED) + l.m(12.0);
    let pct = Pen::style(Face::Mono, l.text.body, TEXT);
    right -= p.right(right, base, &format!("{}%", s.test_percent.min(100)), pct) + l.m(14.0);
    let how = match s.mode {
        Mode::Split => format!("split over {} cpus", s.cpus),
        Mode::Single => String::from("single core"),
    };
    right -= p.right(right, base, &how, small) + l.m(14.0);
    p.fit(x, base, s.test, Pen::style(Face::SemiBold, l.text.body, TEXT), right - x);
    let fill = if s.paused { Fill::Solid(WARN.over(TRACK, 170)) } else { Fill::Solid(ACCENT.over(TRACK, 190)) };
    p.bar(px(r.x), bar_y, px(r.w), bar_h, percent_permille(s.test_percent), fill);
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
    let kinds: Vec<TestKind> = TestKind::sequence(s.preset).collect();
    let records: Vec<&TestRecord> = f.snapshot.history.latest_tests().collect();
    let current = s.position.0.checked_sub(1).filter(|_| s.position.1 > 0);

    let row_h = l.row_h;
    let faults_r = px(r.right());
    let rate_r = faults_r - l.m(70.0);
    let time_r = rate_r - l.m(96.0);
    let name_x = px(r.x) + l.m(30.0);
    let name_max = time_r - l.m(76.0) - name_x;

    let head = p.mid(l.text.label, r.y, row_h);
    p.label(px(r.x) + l.m(8.0), head, "#", DIM);
    p.label(name_x, head, "test", DIM);
    for (right, text) in [(time_r, "time"), (rate_r, "rate"), (faults_r, "faults")] {
        let w = p.label_width(text);
        p.label(right - w, head, text, DIM);
    }

    let visible = ((r.h.saturating_sub(row_h)) / row_h.max(1)) as usize;
    let start = window(kinds.len(), visible, current.unwrap_or(0));
    let mono = Pen::style(Face::Mono, l.text.mono, TEXT);
    for (slot, (i, kind)) in kinds.iter().enumerate().skip(start).take(visible).enumerate() {
        let top = r.y + row_h + u32::try_from(slot).unwrap_or(0) * row_h;
        let row = match current {
            Some(c) if i < c => Row::Done,
            Some(c) if i == c => Row::Current,
            _ => Row::Pending,
        };
        if row == Row::Current {
            p.canvas.round_rect(px(r.x), px(top) + 1.0, px(r.w), px(row_h) - 2.0, l.m(5.0), Fill::Solid(ROW));
            p.canvas.round_rect(
                px(r.x),
                px(top) + l.m(4.0),
                l.m(3.0),
                px(row_h) - l.m(8.0),
                l.m(1.5),
                Fill::Solid(ACCENT),
            );
        }
        let base = p.mid(l.text.body, top, row_h);
        let (ink, sub) = match row {
            Row::Done => (TEXT, MUTED),
            Row::Current => (TEXT, ACCENT),
            Row::Pending => (DIM, DIM),
        };
        p.right(name_x - l.m(10.0), base, &format!("{}", i + 1), Style { color: sub, ..mono });
        let face = if row == Row::Current { Face::SemiBold } else { Face::Regular };
        p.fit(name_x, base, kind.name(), Pen::style(face, l.text.body, ink), name_max);

        let record = records.iter().find(|rec| rec.test == kind.name());
        let values = Style { color: if row == Row::Pending { DIM } else { TEXT }, ..mono };
        let dash = Style { color: DIM, ..mono };
        if row == Row::Current {
            let (word, color) = if s.paused { ("paused", WARN) } else { ("running", ACCENT) };
            p.right(time_r, base, word, Pen::style(Face::Regular, l.text.small, color));
        } else {
            match record {
                Some(rec) => p.right(time_r, base, &format::took(rec.took), values),
                None => p.right(time_r, base, "-", dash),
            };
        }
        match record.and_then(|rec| rec.rate()) {
            Some(rate) if row != Row::Current => p.right(rate_r, base, &format::rate(rate), values),
            _ => p.right(rate_r, base, "-", dash),
        };
        match record {
            Some(rec) if rec.faults > 0 => {
                p.right(faults_r, base, &format!("{}", rec.faults), Style { face: Face::MonoBold, color: BAD, ..mono })
            },
            Some(_) if row != Row::Current => p.right(faults_r, base, "0", Style { color: DIM, ..mono }),
            _ => p.right(faults_r, base, "-", dash),
        };
    }
}

pub fn faults(p: &mut Pen<'_>, r: Rect, f: &Frame<'_>) {
    let s = f.status();
    let l = *p.l;
    if s.faults == 0 {
        let d = l.m(30.0).min(px(r.h));
        let cy = px(r.y) + (px(r.h).min(l.m(56.0))) / 2.0;
        p.canvas.circle(px(r.x) + d / 2.0, cy, d / 2.0, GOOD);
        p.canvas.circle(px(r.x) + d / 2.0, cy, d / 2.0 - l.m(3.5), CARD);
        p.canvas.circle(px(r.x) + d / 2.0, cy, d / 5.0, GOOD);
        let x = px(r.x) + d + l.m(14.0);
        let title = Pen::style(Face::SemiBold, l.text.title, TEXT);
        p.text(x, cy - l.m(3.0), "No faults found", title);
        let note = match s.passes {
            0 => format!("first pass running over {}", format::size(s.bytes)),
            1 => format!("1 full pass over {}, every word read back as written", format::size(s.bytes)),
            n => format!("{n} full passes over {}, every word read back as written", format::size(s.bytes)),
        };
        let small = Pen::style(Face::Regular, l.text.small, MUTED);
        p.fit(x, cy + l.m(16.0), &note, small, px(r.right()) - x);
        return;
    }

    let row_h = l.row_h;
    let mut body = r;
    let top = body.take_top(row_h + row_h / 2);
    let base = p.mid(l.text.title, top.y, top.h);
    let mut x = px(r.x);
    let count = format!("{} faulty word{}", s.faults, if s.faults == 1 { "" } else { "s" });
    x += p.text(x, base, &count, Pen::style(Face::Bold, l.text.title, BAD)) + l.m(12.0);
    let analysis = format!("{}", s.analysis);
    x += p.text(x, base, &analysis, Pen::style(Face::Regular, l.text.body, TEXT)) + l.m(12.0);
    let first = s.first_fault.map_or_else(String::new, |f| format!("first in {} on cpu {}", f.test, f.cpu));
    p.fit(x, base, &first, Pen::style(Face::Regular, l.text.small, MUTED), px(r.right()) - x);

    let mono = Pen::style(Face::Mono, l.text.mono, TEXT);
    let ch = p.measure("0", mono);
    let gap = l.m(16.0);
    let addr_x = px(r.x);
    let exp_x = addr_x + 12.0 * ch + gap;
    let act_x = exp_x + 16.0 * ch + gap;
    let xor_x = act_x + 16.0 * ch + gap;
    let cpu_r = px(r.right());
    let wide = xor_x + 16.0 * ch + gap + l.m(90.0) + l.m(40.0) <= cpu_r;
    let test_x = if wide { xor_x + 16.0 * ch + gap } else { xor_x };
    let test_max = cpu_r - l.m(44.0) - test_x;

    let head = p.mid(l.text.label, body.y, row_h);
    p.label(addr_x, head, "address", DIM);
    p.label(exp_x, head, "expected", DIM);
    p.label(act_x, head, "read", DIM);
    if wide {
        p.label(xor_x, head, "flipped", DIM);
    }
    p.label(test_x, head, "test", DIM);
    let w = p.label_width("cpu");
    p.label(cpu_r - w, head, "cpu", DIM);
    body.take_top(row_h);

    let rows = (body.h / row_h.max(1)) as usize;
    let kept = f.snapshot.faults.len();
    let shown = if kept > rows { rows.saturating_sub(1) } else { kept };
    for (slot, rec) in f.snapshot.faults.iter().rev().take(shown).enumerate() {
        let top = body.y + u32::try_from(slot).unwrap_or(0) * row_h;
        let base = p.mid(mono.px, top, row_h);
        let fault = rec.fault;
        p.text(addr_x, base, &format!("{:012x}", fault.addr.get()), mono);
        p.text(exp_x, base, &format!("{:016x}", fault.expected), Style { color: MUTED, ..mono });
        hex_diff(p, act_x, base, ch, fault.actual, fault.expected, mono);
        if wide {
            let flipped = fault.actual ^ fault.expected;
            let text = format!("{flipped:016x}");
            for (n, c) in text.chars().enumerate() {
                let color = if c == '0' { DIM } else { BAD };
                p.text(
                    xor_x + px(u32::try_from(n).unwrap_or(0)) * ch,
                    base,
                    c.encode_utf8(&mut [0; 4]),
                    Style { color, ..mono },
                );
            }
        }
        p.fit(test_x, base, rec.test, Pen::style(Face::Regular, l.text.small, MUTED), test_max);
        p.right(cpu_r, base, &format!("{}", rec.cpu), mono);
    }
    if kept > shown {
        let top = body.y + u32::try_from(shown).unwrap_or(0) * row_h;
        let base = p.mid(l.text.small, top, row_h);
        let more = u64::try_from(kept - shown).unwrap_or(0) + s.faults.saturating_sub(u64::try_from(kept).unwrap_or(0));
        let text = format!("and {more} more; the boot-volume log has the first of them in full");
        p.fit(addr_x, base, &text, Pen::style(Face::Regular, l.text.small, DIM), px(r.w));
    }
}

/// `value` in hex with the digits that differ from `reference` in red.
fn hex_diff(p: &mut Pen<'_>, x: f32, base: f32, ch: f32, value: u64, reference: u64, style: Style) {
    let text = format!("{value:016x}");
    let expected = format!("{reference:016x}");
    for (n, (c, e)) in text.chars().zip(expected.chars()).enumerate() {
        let color = if c == e { TEXT } else { BAD };
        let face = if c == e { Face::Mono } else { Face::MonoBold };
        let at = x + px(u32::try_from(n).unwrap_or(0)) * ch;
        p.text(at, base, c.encode_utf8(&mut [0; 4]), Style { face, color, ..style });
    }
}

pub fn net(p: &mut Pen<'_>, r: Rect, f: &Frame<'_>) {
    let l = *p.l;
    let row_h = l.row_h;
    let mut body = r;
    let first = body.take_top(row_h);
    let base = p.mid(l.text.body, first.y, first.h);
    let body_style = Pen::style(Face::Regular, l.text.body, TEXT);
    let small = Pen::style(Face::Regular, l.text.small, MUTED);
    let dot_r = l.m(4.0);
    let Some(net) = f.net else {
        p.canvas.circle(px(r.x) + dot_r, base - l.m(4.5), dot_r, BAD);
        p.text(px(r.x) + l.m(16.0), base, "No network", body_style);
        let big = body.take_top(row_h + row_h / 2);
        let reason = f.net_err.unwrap_or("no network interface found");
        let b = p.mid(l.text.body, big.y, big.h);
        p.fit(px(r.x), b, reason, Pen::style(Face::SemiBold, l.text.body, BAD), px(r.w));
        let hint = body.take_top(row_h);
        let b = p.mid(l.text.small, hint.y, hint.h);
        p.fit(px(r.x), b, "the status page needs a NIC the firmware drives", small, px(r.w));
        return;
    };
    let link = net.link;
    let (state, color) = if link.up { ("Link up", GOOD) } else { ("Link down", BAD) };
    p.canvas.circle(px(r.x) + dot_r, base - l.m(4.5), dot_r, color);
    p.text(px(r.x) + l.m(16.0), base, state, body_style);
    p.right(px(r.right()), base, &format!("{}", link.mac), Style { face: Face::Mono, ..small });

    let big = body.take_top(row_h * 2);
    let url = body.take_top(row_h);
    let rest = body.take_top(row_h);
    if let Some(cidr) = link.address {
        let ip = Pen::style(Face::MonoBold, l.text.big, TEXT);
        let b = p.mid(ip.px, big.y, big.h);
        p.text(px(r.x), b, &format!("{}", cidr.address()), ip);
        let b = p.mid(l.text.body, url.y, url.h);
        let mut x = px(r.x);
        x += p.label(x, b, "open", DIM) + l.m(10.0);
        let page = format!("http://{}/", cidr.address());
        p.fit(x, b, &page, Pen::style(Face::Mono, l.text.body, ACCENT), px(r.right()) - x);
    } else {
        let b = p.mid(l.text.title, big.y, big.h);
        p.text(px(r.x), b, "Waiting for DHCP", Pen::style(Face::SemiBold, l.text.title, WARN));
        let b = p.mid(l.text.small, url.y, url.h);
        p.text(px(r.x), b, "the status page opens once there is an address", small);
    }
    let b = p.mid(l.text.small, rest.y, rest.h);
    let tail = format!(
        "{MDNS_NAME} \u{b7} gateway {} \u{b7} dns {}",
        crate::net::OptAddr(link.gateway),
        crate::net::OptAddr(link.dns)
    );
    p.fit(px(r.x), b, &tail, Style { color: DIM, ..small }, px(r.w));
}

pub fn passes(p: &mut Pen<'_>, r: Rect, f: &Frame<'_>) {
    let s = f.status();
    let l = *p.l;
    let history = f.snapshot.history;
    let mut body = r;
    let line = body.take_top(l.row_h);
    let base = p.mid(l.text.body, line.y, line.h);
    let small = Pen::style(Face::Regular, l.text.small, MUTED);
    let mut x = px(r.x);
    match history.passes.back() {
        None => {
            p.text(x, base, "First pass running", Pen::style(Face::SemiBold, l.text.body, TEXT));
        },
        Some(last) => {
            let count = format!("{} pass{}", s.passes, if s.passes == 1 { "" } else { "es" });
            x += p.text(x, base, &count, Pen::style(Face::SemiBold, l.text.body, TEXT)) + l.m(10.0);
            let best = history.passes.iter().map(|p| p.took).min().unwrap_or_default();
            let worst = history.passes.iter().map(|p| p.took).max().unwrap_or_default();
            let summary = format!(
                "last {} \u{b7} best {} \u{b7} worst {}",
                format::took(last.took),
                format::took(best),
                format::took(worst)
            );
            p.fit(x, base, &summary, small, px(r.right()) - x);
        },
    }

    // Finished passes as bars of their duration, then the running pass growing towards the last
    // one's height as its percentage climbs.
    body.take_top(l.pad);
    let chart = body.take_top(super::layout::whole_px(l.m(150.0)).min(body.h.saturating_sub(l.row_h)));
    let labels = body.take_top(l.row_h);
    let gap = l.m(6.0);
    let max_slots = super::layout::whole_px((px(chart.w) + gap) / (l.m(10.0) + gap)).max(2) as usize;
    let shown: Vec<_> = history.passes.iter().rev().take(max_slots - 1).rev().collect();
    let slots = px(u32::try_from(shown.len() + 1).unwrap_or(1));
    let bar_w = ((px(chart.w) + gap) / slots - gap).clamp(l.m(10.0), l.m(36.0));
    let max = shown.iter().map(|p| p.took.as_secs_f32()).fold(0.0f32, f32::max);
    let floor = px(chart.bottom());
    let full_h = px(chart.h);
    p.canvas.fill(Rect::new(chart.x, chart.bottom(), chart.w, 1), CARD_EDGE);
    let slot_x = |n: usize| px(chart.x) + px(u32::try_from(n).unwrap_or(0)) * (bar_w + gap);
    for (n, pass) in shown.iter().enumerate() {
        let h = (full_h * pass.took.as_secs_f32() / max.max(f32::MIN_POSITIVE)).max(l.m(3.0));
        let fill = if pass.faults > 0 { Fill::Solid(BAD) } else { Fill::Down(ACCENT, ACCENT_FROM.over(CARD, 140)) };
        p.canvas.round_rect(slot_x(n), floor - h, bar_w, h, l.m(3.0), fill);
    }
    let last_h = shown.last().map_or(full_h, |last| full_h * last.took.as_secs_f32() / max.max(f32::MIN_POSITIVE));
    let now_x = slot_x(shown.len());
    p.canvas.round_rect(now_x, floor - last_h, bar_w, last_h, l.m(3.0), Fill::Solid(CHIP));
    let grown = (last_h * f32::from(s.percent.min(100)) / 100.0).max(l.m(3.0));
    p.canvas.round_rect(now_x, floor - grown, bar_w, grown, l.m(3.0), Fill::Solid(ACCENT.over(CARD, 90)));

    let b = p.mid(l.text.small, labels.y, labels.h);
    let dim = Style { color: DIM, ..small };
    let now = format!("{}", s.passes + 1);
    let w = p.measure(&now, dim);
    p.text(now_x + (bar_w - w) / 2.0, b, &now, Style { color: ACCENT, ..small });
    if let Some(first) = shown.first() {
        let text = format!("{}", first.pass + 1);
        let w = p.measure(&text, dim);
        p.text(slot_x(0) + (bar_w - w) / 2.0, b, &text, dim);
    }
}

pub fn cpus(p: &mut Pen<'_>, r: Rect, f: &Frame<'_>) {
    let s = f.status();
    let l = *p.l;
    let list = &f.snapshot.history.cpus;
    let mut body = r;
    let line = body.take_top(l.row_h);
    let base = p.mid(l.text.body, line.y, line.h);
    let small = Pen::style(Face::Regular, l.text.small, MUTED);
    if list.is_empty() {
        p.fit(px(r.x), base, "No MP Services: every test runs on the boot processor", small, px(r.w));
        return;
    }
    let mut cores: Vec<(u32, u32)> = list.iter().map(|c| (c.package, c.core)).collect();
    cores.sort_unstable();
    cores.dedup();
    let mut packages: Vec<u32> = list.iter().map(|c| c.package).collect();
    packages.sort_unstable();
    packages.dedup();
    let mut x = px(r.x);
    x += p.text(x, base, &format!("{} logical", list.len()), Pen::style(Face::SemiBold, l.text.body, TEXT)) + l.m(10.0);
    let shape = format!(
        "{} core{} \u{b7} {} package{}",
        cores.len(),
        if cores.len() == 1 { "" } else { "s" },
        packages.len(),
        if packages.len() == 1 { "" } else { "s" }
    );
    x += p.text(x, base, &shape, small) + l.m(10.0);
    let active = match s.mode {
        Mode::Split => s.cpus.min(list.len()),
        Mode::Single => 1,
    };
    let note = match s.mode {
        Mode::Split => format!("{active} running this test"),
        Mode::Single => String::from("this test runs on the boot processor"),
    };
    let accent = Pen::style(Face::Regular, l.text.small, if s.mode == Mode::Split { ACCENT } else { MUTED });
    let w = p.measure(&note, accent);
    if x + w <= px(r.right()) {
        p.right(px(r.right()), base, &note, accent);
    }

    body.take_top(l.pad / 2);
    let gap = super::layout::whole_px(l.m(5.0)).max(2);
    let n = u32::try_from(list.len()).unwrap_or(u32::MAX);
    let largest = super::layout::whole_px(l.m(30.0));
    let smallest = super::layout::whole_px(l.m(10.0)).max(6);
    let (side, cols) = tile_grid(n, body.w, body.h, gap, largest, smallest);
    for (i, cpu) in list.iter().enumerate() {
        let i = u32::try_from(i).unwrap_or(0);
        let (col, row) = (i % cols, i / cols);
        let (x, y) = (body.x + col * (side + gap), body.y + row * (side + gap));
        if y + side > body.bottom() {
            break;
        }
        let on = (i as usize) < active;
        let fill = if on { Fill::Down(ACCENT.over(CARD, 110), ACCENT.over(CARD, 60)) } else { Fill::Solid(CHIP) };
        p.canvas.round_rect(px(x), px(y), px(side), px(side), l.m(5.0), fill);
        if cpu.bsp {
            p.canvas.circle(px(x + side) - l.m(5.0), px(y) + l.m(5.0), l.m(2.2), if on { TEXT } else { MUTED });
        }
        let label_px = u16::try_from(side * 2 / 5).unwrap_or(u16::MAX).clamp(6, l.text.mono);
        let style = Pen::style(Face::Mono, label_px, if on { TEXT } else { MUTED });
        let label = format!("{}", cpu.number);
        let w = p.measure(&label, style);
        let b = p.mid(label_px, y, side);
        p.text(px(x) + (px(side) - w) / 2.0, b, &label, style);
    }
}

pub fn hints(p: &mut Pen<'_>, r: Rect, f: &Frame<'_>) {
    let s = f.status();
    let l = *p.l;
    let keys = [("P", if s.paused { "resume" } else { "pause" }), ("R", "restart"), ("Q", "quit")];
    let cap_style = Pen::style(Face::MonoBold, l.text.small, TEXT);
    let text_style = Pen::style(Face::Regular, l.text.small, MUTED);
    let h = l.m(22.0).min(px(r.h));
    let y = px(r.y) + (px(r.h) - h) / 2.0;
    let mut x = px(r.x) + l.m(4.0);
    for (key, what) in keys {
        let w = h.max(p.measure(key, cap_style) + l.m(12.0));
        p.canvas.round_rect(x, y, w, h, l.m(5.0), Fill::Solid(CARD_EDGE));
        p.canvas.round_rect(x + 1.0, y + 1.0, w - 2.0, h - 2.5, l.m(4.0), Fill::Solid(CHIP));
        let base = p.fonts.centred_baseline(cap_style.px, y, h);
        let kw = p.measure(key, cap_style);
        p.text(x + (w - kw) / 2.0, base, key, cap_style);
        x += w + l.m(8.0);
        x += p.text(x, base, what, text_style) + l.m(22.0);
    }
    let base = p.mid(l.text.small, r.y, r.h);
    let version = concat!("ferrite-uefi ", env!("CARGO_PKG_VERSION"));
    p.right(px(r.right()) - l.m(4.0), base, version, Style { color: DIM, ..text_style });
}
