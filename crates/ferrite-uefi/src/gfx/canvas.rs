//! The opaque back buffer the dashboard draws into: exact fills, anti-aliased shapes through
//! tiny-skia, and glyphs from [`Fonts`] blended in.
//!
//! The pixels are BGRA, the order of a `Blt` pixel and of most framebuffers, so pushing them needs
//! no conversion; tiny-skia believes it draws RGBA, so every colour handed to it has red and blue
//! swapped.
//!
//! A widget is drawn in two steps. Between [`Canvas::begin`] and [`Canvas::finish`] the drawing
//! calls only record what to draw. `finish` compares the recording with the widget's previous one,
//! redraws just the parts around the steps that differ into a small scratch buffer, and copies to
//! the canvas only the pixels that changed, so a ticking number costs about its own area in memory
//! traffic rather than its widget's. Outside that pair the calls draw straight onto the canvas.

#![expect(clippy::many_single_char_names, reason = "x, y, w, h and r are the geometry of a shape")]

use alloc::string::String;
use alloc::vec::Vec;

use tiny_skia::{Color, FillRule, GradientStop, LinearGradient, Paint, PathBuilder, Pixmap, PixmapMut, Point};
use tiny_skia::{Shader, SpreadMode, Transform};

use super::rect::{Plane, changed_bands};
use super::text::{ELLIPSIS, Fonts, Glyph, Ink, Style};
use super::{DirtyRects, Rect, ceil_px, px, whole_px};

/// An opaque colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    /// From `0xRRGGBB`.
    #[must_use]
    pub const fn hex(rgb: u32) -> Self {
        let [_, r, g, b] = rgb.to_be_bytes();
        Self { r, g, b }
    }

    /// This colour at `alpha` (0 to 255) over `under`.
    #[must_use]
    pub const fn over(self, under: Self, alpha: u8) -> Self {
        Self { r: mix(under.r, self.r, alpha), g: mix(under.g, self.g, alpha), b: mix(under.b, self.b, alpha) }
    }

    /// As tiny-skia must see it to land in BGRA order.
    fn color(self) -> Color {
        Color::from_rgba8(self.b, self.g, self.r, 255)
    }

    /// As a canvas pixel.
    const fn pixel(self) -> [u8; 4] {
        [self.b, self.g, self.r, 255]
    }
}

/// How a shape is filled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fill {
    Solid(Rgb),
    /// Left to right.
    Across(Rgb, Rgb),
    /// Top to bottom.
    Down(Rgb, Rgb),
}

const fn mix(under: u8, over: u8, alpha: u8) -> u8 {
    let (under, over, alpha) = (under as u16, over as u16, alpha as u16);
    ((under * (255 - alpha) + over * alpha + 127) / 255) as u8
}

/// Coverage lifted towards opaque. Light text blended in sRGB space reads thin and grey on a dark
/// ground; this is close to a 1/1.5 gamma on coverage without needing `powf`.
const BOOST: [u8; 256] = {
    let mut table = [0; 256];
    let mut coverage = 0;
    while coverage < 256 {
        let a = coverage as u16;
        table[coverage] = (a + a * (255 - a) / 512) as u8;
        coverage += 1;
    }
    table
};

/// A piece of a recording's text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Span {
    start: u32,
    len: u32,
}

/// One drawing step.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Op {
    Fill {
        rect: Rect,
        color: Rgb,
    },
    Round {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius: f32,
        fill: Fill,
    },
    Circle {
        cx: f32,
        cy: f32,
        r: f32,
        color: Rgb,
    },
    /// `x` and `base` are the run's rounded origin.
    Text {
        x: i32,
        base: i32,
        text: Span,
        style: Style,
    },
}

/// A step and the pixels it can touch.
#[derive(Clone, Copy, Debug)]
struct Item {
    op: Op,
    bbox: Rect,
}

/// What one widget drew in a frame.
#[derive(Debug)]
pub struct Recording {
    items: Vec<Item>,
    /// The text of every [`Op::Text`], back to back.
    text: String,
    /// The widget's rectangle; nothing is drawn outside it.
    rect: Rect,
    /// What the rectangle is cleared to before the steps run.
    ground: Rgb,
}

impl Recording {
    /// Room for the steps and text of a typical widget, so redraws do not grow it.
    const ITEMS: usize = 768;
    const TEXT: usize = 4096;

    #[must_use]
    pub const fn new() -> Self {
        Self { items: Vec::new(), text: String::new(), rect: Rect::new(0, 0, 0, 0), ground: Rgb::hex(0) }
    }

    /// An empty recording that has the room already.
    #[must_use]
    pub fn roomy() -> Self {
        Self { items: Vec::with_capacity(Self::ITEMS), text: String::with_capacity(Self::TEXT), ..Self::new() }
    }

    fn slice(&self, span: Span) -> &str {
        &self.text[span.start as usize..(span.start + span.len) as usize]
    }
}

impl Default for Recording {
    fn default() -> Self {
        Self::new()
    }
}

impl Item {
    /// Whether `other`, from the recording `theirs`, draws exactly what this one does.
    fn same(&self, mine: &Recording, other: &Self, theirs: &Recording) -> bool {
        match (self.op, other.op) {
            (Op::Text { x, base, text, style }, Op::Text { x: x2, base: base2, text: text2, style: style2 }) => {
                let (ours, theirs) = (mine.slice(text), theirs.slice(text2));
                x == x2 && base == base2 && style == style2 && ours == theirs
            },
            (a, b) => a == b,
        }
    }
}

impl Op {
    /// The same step, with a rounded rectangle's width zeroed.
    fn without_width(self) -> Self {
        match self {
            Self::Round { x, y, h, radius, fill, .. } => Self::Round { x, y, w: 0.0, h, radius, fill },
            op => op,
        }
    }
}

/// The columns two versions of a solid bar can differ in, when only its right end moved: its left
/// end and straight edges are the same pixels in both.
fn bar_growth(old: &Item, new: &Item) -> Option<Rect> {
    let (Op::Round { x, w, h, radius, fill: Fill::Solid(_), .. }, Op::Round { w: w2, .. }) = (old.op, new.op) else {
        return None;
    };
    let cap = radius.min(h / 2.0);
    if old.op.without_width() != new.op.without_width() || w.min(w2) < 2.0 * cap {
        return None;
    }
    let both = old.bbox.union(new.bbox);
    let from = whole_px(x + w.min(w2) - cap - 2.0);
    let to = ceil_px(x + w.max(w2) + 2.0);
    Some(Rect::new(from, both.y, to.saturating_sub(from), both.h).intersect(both))
}

/// The pixels a shape within `x..x + w`, `y..y + h` can colour, with a pixel to spare for the
/// anti-aliased edge.
fn shape_box(x: f32, y: f32, w: f32, h: f32) -> Rect {
    let (left, top) = (whole_px(x - 1.0), whole_px(y - 1.0));
    let (right, bottom) = (ceil_px(x + w + 1.0), ceil_px(y + h + 1.0));
    Rect::new(left, top, right.saturating_sub(left), bottom.saturating_sub(top))
}

/// `ink` of a run at `x`, `base`, as a rectangle.
fn ink_box(ink: Ink, x: i32, base: i32) -> Rect {
    if ink.is_empty() {
        return Rect::default();
    }
    let at = |v: i32| u32::try_from(v).unwrap_or(0);
    let (left, top) = (at(x + ink.left), at(base + ink.top));
    Rect::new(left, top, at(x + ink.right).saturating_sub(left), at(base + ink.bottom).saturating_sub(top))
}

pub struct Canvas {
    pixmap: Pixmap,
    /// The part of a widget being redrawn, packed rows.
    scratch: Vec<u8>,
    /// The widget being drawn, between [`Canvas::begin`] and [`Canvas::finish`].
    rec: Recording,
    recording: bool,
    /// The parts of the widget to redraw.
    regions: DirtyRects,
    /// Memory for the paths of shapes, kept between them.
    path: Option<PathBuilder>,
}

impl Canvas {
    /// `None` when the size is zero or the buffer cannot be allocated.
    #[must_use]
    pub fn new(width: u32, height: u32) -> Option<Self> {
        let pixmap = Pixmap::new(width, height)?;
        Some(Self {
            pixmap,
            scratch: Vec::new(),
            rec: Recording::roomy(),
            recording: false,
            regions: DirtyRects::new(Rect::new(0, 0, width, height)),
            path: Some(PathBuilder::new()),
        })
    }

    #[must_use]
    pub fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.pixmap.width(), self.pixmap.height())
    }

    /// The pixels, BGRA8 rows of [`Canvas::bounds`]' width, alpha always opaque.
    #[must_use]
    pub fn bgra(&self) -> &[u8] {
        self.pixmap.data()
    }

    /// Start recording a widget in `rect`, which is cleared to `ground` wherever it is redrawn.
    pub fn begin(&mut self, rect: Rect, ground: Rgb) {
        self.rec.items.clear();
        self.rec.text.clear();
        self.rec.rect = rect.intersect(self.bounds());
        self.rec.ground = ground;
        self.recording = true;
    }

    /// End the recording and draw what differs from `last`, the widget's previous recording,
    /// which then holds this one. With `redraw_all` the whole rectangle is drawn. The pixels that
    /// changed are added to `dirty`, as rectangles of one band each at most `band` pixels wide.
    pub fn finish(
        &mut self,
        fonts: &mut Fonts,
        last: &mut Recording,
        redraw_all: bool,
        dirty: &mut DirtyRects,
        band: u32,
    ) {
        self.recording = false;
        self.regions.clear();
        let all = redraw_all || last.rect != self.rec.rect || last.ground != self.rec.ground;
        if all {
            self.regions.add(self.rec.rect);
        } else {
            let n = last.items.len().max(self.rec.items.len());
            for i in 0..n {
                match (last.items.get(i), self.rec.items.get(i)) {
                    (Some(old), Some(new)) if old.same(last, new, &self.rec) => {},
                    (Some(old), Some(new)) => {
                        if let Some(span) = bar_growth(old, new) {
                            self.regions.add(span);
                        } else {
                            self.regions.add(old.bbox);
                            self.regions.add(new.bbox);
                        }
                    },
                    (Some(item), None) | (None, Some(item)) => self.regions.add(item.bbox),
                    (None, None) => {},
                }
            }
        }
        for i in 0..self.regions.as_slice().len() {
            let region = self.regions.as_slice()[i].intersect(self.rec.rect);
            if region.is_empty() {
                continue;
            }
            self.compose(fonts, region);
            if all {
                self.commit(region, &[region], dirty);
            } else {
                let width = self.pixmap.width() as usize;
                let old = Plane {
                    data: &self.pixmap.data()[(region.y as usize * width + region.x as usize) * 4..],
                    stride: width * 4,
                };
                let new = Plane { data: &self.scratch, stride: region.w as usize * 4 };
                let changed = changed_bands(old, new, region, band);
                self.commit(region, &changed, dirty);
            }
        }
        core::mem::swap(last, &mut self.rec);
    }

    /// Draw the recording's steps over `region` into the scratch buffer.
    fn compose(&mut self, fonts: &mut Fonts, region: Rect) {
        let len = region.w as usize * region.h as usize * 4;
        if self.scratch.len() < len {
            self.scratch.resize(len, 0);
        }
        let ground = self.rec.ground.pixel();
        let scratch = &mut self.scratch[..len];
        scratch.as_chunks_mut::<4>().0.fill(ground);
        let Some(mut dst) = PixmapMut::from_bytes(scratch, region.w, region.h) else { return };
        let mut painter = Painter { dst: &mut dst, origin: (region.x, region.y), clip: region, path: &mut self.path };
        for item in &self.rec.items {
            if item.bbox.intersect(region).is_empty() {
                continue;
            }
            match item.op {
                Op::Text { x, base, text, style } => painter.text(fonts, x, base, self.rec.slice(text), style),
                op => painter.shape(op),
            }
        }
    }

    /// Copy `rects`, inside `region`, from the scratch buffer to the canvas and mark them dirty.
    fn commit(&mut self, region: Rect, rects: &[Rect], dirty: &mut DirtyRects) {
        let width = self.pixmap.width() as usize;
        let data = self.pixmap.data_mut();
        for rect in rects {
            let bytes = rect.w as usize * 4;
            for y in rect.y..rect.bottom() {
                let from = ((y - region.y) as usize * region.w as usize + (rect.x - region.x) as usize) * 4;
                let to = (y as usize * width + rect.x as usize) * 4;
                data[to..to + bytes].copy_from_slice(&self.scratch[from..from + bytes]);
            }
            dirty.add(*rect);
        }
    }

    /// Fill whole pixels, exactly; clipped to the canvas.
    pub fn fill(&mut self, rect: Rect, color: Rgb) {
        let rect = rect.intersect(self.bounds());
        self.step(Op::Fill { rect, color }, rect);
    }

    /// An anti-aliased rectangle with corners of `radius`.
    pub fn round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, fill: Fill) {
        self.step(Op::Round { x, y, w, h, radius, fill }, shape_box(x, y, w, h));
    }

    pub fn circle(&mut self, cx: f32, cy: f32, r: f32, color: Rgb) {
        self.step(Op::Circle { cx, cy, r, color }, shape_box(cx - r, cy - r, 2.0 * r, 2.0 * r));
    }

    /// Record `op`, or draw it now outside a recording.
    fn step(&mut self, op: Op, bbox: Rect) {
        if self.recording {
            self.rec.items.push(Item { op, bbox });
            return;
        }
        let bounds = self.bounds();
        let mut dst = self.pixmap.as_mut();
        Painter { dst: &mut dst, origin: (0, 0), clip: bounds, path: &mut self.path }.shape(op);
    }

    /// Draw `text` with its origin at `x` on `baseline`; returns its advance.
    pub fn text(&mut self, fonts: &mut Fonts, x: f32, baseline: f32, text: &str, style: Style) -> f32 {
        let run = fonts.run(text, style);
        self.put(fonts, super::text::round(x), super::text::round(baseline), text, style, run.ink);
        run.advance
    }

    /// Like [`Canvas::text`], cut with an ellipsis to fit in `max` pixels.
    pub fn text_fit(&mut self, fonts: &mut Fonts, x: f32, baseline: f32, text: &str, style: Style, max: f32) -> f32 {
        let (prefix, cut) = fonts.fit(text, style, max);
        let mut width = self.text(fonts, x, baseline, prefix, style);
        if cut {
            let mut buf = [0; 4];
            width += self.text(fonts, x + width, baseline, ELLIPSIS.encode_utf8(&mut buf), style);
        }
        width
    }

    /// Like [`Canvas::text`], ending at `right`.
    pub fn text_right(&mut self, fonts: &mut Fonts, right: f32, baseline: f32, text: &str, style: Style) -> f32 {
        let run = fonts.run(text, style);
        self.put(fonts, super::text::round(right - run.advance), super::text::round(baseline), text, style, run.ink);
        run.advance
    }

    /// Record the run at the rounded origin `x`, `base`, or draw it now outside a recording.
    fn put(&mut self, fonts: &mut Fonts, x: i32, base: i32, text: &str, style: Style, ink: Ink) {
        if !self.recording {
            let bounds = self.bounds();
            let mut dst = self.pixmap.as_mut();
            Painter { dst: &mut dst, origin: (0, 0), clip: bounds, path: &mut self.path }
                .text(fonts, x, base, text, style);
            return;
        }
        let bbox = ink_box(ink, x, base).intersect(self.rec.rect);
        if bbox.is_empty() {
            return;
        }
        let start = u32::try_from(self.rec.text.len()).unwrap_or(u32::MAX);
        self.rec.text.push_str(text);
        let text = Span { start, len: u32::try_from(text.len()).unwrap_or(0) };
        self.rec.items.push(Item { op: Op::Text { x, base, text, style }, bbox });
    }
}

/// Draws steps into `dst`, which shows the canvas from `origin`, inside `clip` (in canvas
/// coordinates).
struct Painter<'a, 'p> {
    dst: &'a mut PixmapMut<'p>,
    origin: (u32, u32),
    clip: Rect,
    path: &'a mut Option<PathBuilder>,
}

impl Painter<'_, '_> {
    fn shape(&mut self, op: Op) {
        match op {
            Op::Fill { rect, color } => self.fill(rect, color),
            Op::Round { x, y, w, h, radius, fill } => self.round_rect(x, y, w, h, radius, fill),
            Op::Circle { cx, cy, r, color } => self.circle(cx, cy, r, color),
            Op::Text { .. } => {},
        }
    }

    fn fill(&mut self, rect: Rect, color: Rgb) {
        let rect = rect.intersect(self.clip);
        let width = self.dst.width() as usize;
        let (ox, oy) = self.origin;
        let px = color.pixel();
        let data = self.dst.data_mut();
        for y in rect.y..rect.bottom() {
            let start = ((y - oy) as usize * width + (rect.x - ox) as usize) * 4;
            data[start..start + rect.w as usize * 4].as_chunks_mut::<4>().0.fill(px);
        }
    }

    /// Shapes are built in canvas coordinates and then moved, which is exact, so the pixels they
    /// colour do not depend on where `origin` is.
    fn offset(&self) -> Transform {
        Transform::from_translate(-px(self.origin.0), -px(self.origin.1))
    }

    fn gradient(&self, start: Point, end: Point, from: Rgb, to: Rgb) -> Shader<'static> {
        let (dx, dy) = (px(self.origin.0), px(self.origin.1));
        let (start, end) = (Point::from_xy(start.x - dx, start.y - dy), Point::from_xy(end.x - dx, end.y - dy));
        let stops = alloc::vec![GradientStop::new(0.0, from.color()), GradientStop::new(1.0, to.color())];
        LinearGradient::new(start, end, stops, SpreadMode::Pad, Transform::identity())
            .unwrap_or_else(|| Shader::SolidColor(from.color()))
    }

    /// Fill the path built by `build` with `shader`, anti-aliased.
    fn paint(&mut self, build: impl FnOnce(&mut PathBuilder), shader: Shader<'_>) {
        let mut builder = self.path.take().unwrap_or_default();
        build(&mut builder);
        let Some(path) = builder.finish().and_then(|path| path.transform(self.offset())) else { return };
        let paint = Paint { shader, anti_alias: true, ..Paint::default() };
        self.dst.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
        *self.path = Some(path.clear());
    }

    fn round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, fill: Fill) {
        let shader = match fill {
            Fill::Solid(c) => Shader::SolidColor(c.color()),
            Fill::Across(from, to) => self.gradient(Point::from_xy(x, y), Point::from_xy(x + w, y), from, to),
            Fill::Down(from, to) => self.gradient(Point::from_xy(x, y), Point::from_xy(x, y + h), from, to),
        };
        self.paint(|pb| push_round_rect(pb, x, y, w, h, radius), shader);
    }

    fn circle(&mut self, cx: f32, cy: f32, r: f32, color: Rgb) {
        self.paint(|pb| pb.push_circle(cx, cy, r), Shader::SolidColor(color.color()));
    }

    fn text(&mut self, fonts: &mut Fonts, x: i32, base: i32, text: &str, style: Style) {
        fonts.layout(text, style, |at, glyph| self.blend(x + at.x, base + at.top, glyph, style.color));
    }

    /// Blend `glyph` with its top left at `left`, `top` (canvas coordinates) over the clip.
    fn blend(&mut self, left: i32, top: i32, glyph: &Glyph, color: Rgb) {
        let (w, h) = (glyph.metrics.width, glyph.metrics.height);
        let bound = |v: u32| i32::try_from(v).unwrap_or(i32::MAX);
        let to_i32 = |v: usize| i32::try_from(v).unwrap_or(i32::MAX);
        let (x0, x1) = (left.max(bound(self.clip.x)), (left + to_i32(w)).min(bound(self.clip.right())));
        let (y0, y1) = (top.max(bound(self.clip.y)), (top + to_i32(h)).min(bound(self.clip.bottom())));
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let at = |v: i32| usize::try_from(v).unwrap_or(0);
        let stride = self.dst.width() as usize;
        let (ox, oy) = (bound(self.origin.0), bound(self.origin.1));
        let span = at(x1 - x0);
        let data = self.dst.data_mut();
        for y in y0..y1 {
            let covered = &glyph.coverage[at(y - top) * w + at(x0 - left)..][..span];
            let start = (at(y - oy) * stride + at(x0 - ox)) * 4;
            let pixels = data[start..start + span * 4].as_chunks_mut::<4>().0;
            for (&coverage, px) in covered.iter().zip(pixels) {
                if coverage == 0 {
                    continue;
                }
                let a = BOOST[coverage as usize];
                px[0] = mix(px[0], color.b, a);
                px[1] = mix(px[1], color.g, a);
                px[2] = mix(px[2], color.r, a);
            }
        }
    }
}

fn push_round_rect(pb: &mut PathBuilder, x: f32, y: f32, w: f32, h: f32, radius: f32) {
    let r = radius.min(w / 2.0).min(h / 2.0).max(0.0);
    // Control-point distance of the usual four-cubic circle.
    let k = r * 0.552_284_8;
    let (right, bottom) = (x + w, y + h);
    pb.move_to(x + r, y);
    pb.line_to(right - r, y);
    pb.cubic_to(right - r + k, y, right, y + r - k, right, y + r);
    pb.line_to(right, bottom - r);
    pb.cubic_to(right, bottom - r + k, right - r + k, bottom, right - r, bottom);
    pb.line_to(x + r, bottom);
    pb.cubic_to(x + r - k, bottom, x, bottom - r + k, x, bottom - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
}
