//! The opaque back buffer the dashboard draws into: exact fills, anti-aliased shapes through
//! tiny-skia, and glyphs from [`Fonts`] blended in.
//!
//! The pixels are BGRA, the order of a `Blt` pixel and of most framebuffers, so pushing them needs
//! no conversion; tiny-skia believes it draws RGBA, so every colour handed to it has red and blue
//! swapped.

#![expect(clippy::many_single_char_names, reason = "x, y, w, h and r are the geometry of a shape")]

use alloc::vec;
use alloc::vec::Vec;

use tiny_skia::{Color, FillRule, GradientStop, LinearGradient, Paint, Path, PathBuilder, Pixmap, Point, Shader};
use tiny_skia::{SpreadMode, Transform};

use super::Rect;
use super::text::{ELLIPSIS, Fonts, Glyph, Style};

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
const fn boost(coverage: u8) -> u8 {
    let a = coverage as u16;
    (a + a * (255 - a) / 512) as u8
}

pub struct Canvas {
    pixmap: Pixmap,
    /// Glyphs are clipped to this; set to the widget being drawn.
    clip: Rect,
}

impl Canvas {
    /// `None` when the size is zero or the buffer cannot be allocated.
    #[must_use]
    pub fn new(width: u32, height: u32) -> Option<Self> {
        let pixmap = Pixmap::new(width, height)?;
        Some(Self { pixmap, clip: Rect::new(0, 0, width, height) })
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

    /// Copy the pixels of `rect` into `out`, rows packed.
    pub fn copy_out(&self, rect: Rect, out: &mut Vec<u8>) {
        let rect = rect.intersect(self.bounds());
        let stride = self.pixmap.width() as usize * 4;
        out.clear();
        for y in rect.y..rect.bottom() {
            let start = y as usize * stride + rect.x as usize * 4;
            out.extend_from_slice(&self.pixmap.data()[start..start + rect.w as usize * 4]);
        }
    }

    pub fn set_clip(&mut self, clip: Rect) {
        self.clip = clip.intersect(self.bounds());
    }

    /// Fill whole pixels, exactly; clipped to the canvas, not to the clip.
    pub fn fill(&mut self, rect: Rect, color: Rgb) {
        let rect = rect.intersect(self.bounds());
        let stride = self.pixmap.width() as usize * 4;
        let px = [color.b, color.g, color.r, 255];
        let row = vec![px; rect.w as usize].concat();
        let data = self.pixmap.data_mut();
        for y in rect.y..rect.bottom() {
            let start = y as usize * stride + rect.x as usize * 4;
            data[start..start + row.len()].copy_from_slice(&row);
        }
    }

    /// An anti-aliased rectangle with corners of `radius`.
    pub fn round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, fill: Fill) {
        let Some(path) = round_rect_path(x, y, w, h, radius) else { return };
        let shader = match fill {
            Fill::Solid(c) => Shader::SolidColor(c.color()),
            Fill::Across(from, to) => gradient(Point::from_xy(x, y), Point::from_xy(x + w, y), from, to),
            Fill::Down(from, to) => gradient(Point::from_xy(x, y), Point::from_xy(x, y + h), from, to),
        };
        let paint = Paint { shader, anti_alias: true, ..Paint::default() };
        self.pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    }

    pub fn circle(&mut self, cx: f32, cy: f32, r: f32, color: Rgb) {
        let Some(path) = PathBuilder::from_circle(cx, cy, r) else { return };
        let paint = Paint { shader: Shader::SolidColor(color.color()), anti_alias: true, ..Paint::default() };
        self.pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    }

    /// Draw `text` with its origin at `x` on `baseline`; returns its advance.
    pub fn text(&mut self, fonts: &mut Fonts, x: f32, baseline: f32, text: &str, style: Style) -> f32 {
        let (x0, base) = (super::text::round(x), super::text::round(baseline));
        fonts.layout(text, style, |at, glyph| self.blend(x0 + at.x, base + at.top, glyph, style.color))
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
        let width = fonts.measure(text, style);
        self.text(fonts, right - width, baseline, text, style)
    }

    fn blend(&mut self, left: i32, top: i32, glyph: &Glyph, color: Rgb) {
        let (w, h) = (glyph.metrics.width, glyph.metrics.height);
        let clip = self.clip;
        let stride = self.pixmap.width() as usize;
        let data = self.pixmap.data_mut();
        for (row, line) in glyph.coverage.chunks_exact(w.max(1)).enumerate().take(h) {
            let Some(y) = inside(top, row, clip.y, clip.bottom()) else { continue };
            for (col, &coverage) in line.iter().enumerate() {
                let Some(x) = inside(left, col, clip.x, clip.right()) else { continue };
                if coverage == 0 {
                    continue;
                }
                let a = boost(coverage);
                let i = (y * stride + x) * 4;
                let px = &mut data[i..i + 3];
                px[0] = mix(px[0], color.b, a);
                px[1] = mix(px[1], color.g, a);
                px[2] = mix(px[2], color.r, a);
            }
        }
    }
}

/// `origin + offset` when it falls in `lo..hi`.
fn inside(origin: i32, offset: usize, lo: u32, hi: u32) -> Option<usize> {
    let v = i64::from(origin) + i64::try_from(offset).ok()?;
    if v < i64::from(lo) || v >= i64::from(hi) {
        return None;
    }
    usize::try_from(v).ok()
}

fn gradient(start: Point, end: Point, from: Rgb, to: Rgb) -> Shader<'static> {
    let stops = vec![GradientStop::new(0.0, from.color()), GradientStop::new(1.0, to.color())];
    LinearGradient::new(start, end, stops, SpreadMode::Pad, Transform::identity())
        .unwrap_or_else(|| Shader::SolidColor(from.color()))
}

fn round_rect_path(x: f32, y: f32, w: f32, h: f32, radius: f32) -> Option<Path> {
    let r = radius.min(w / 2.0).min(h / 2.0).max(0.0);
    // Control-point distance of the usual four-cubic circle.
    let k = r * 0.552_284_8;
    let (right, bottom) = (x + w, y + h);
    let mut pb = PathBuilder::new();
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
    pb.finish()
}
