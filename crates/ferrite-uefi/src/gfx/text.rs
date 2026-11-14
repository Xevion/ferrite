//! The embedded faces, a cache of rasterized glyphs, and text measuring and fitting.
//!
//! Every glyph's position is decided in [`Fonts::layout`]; it rounds the pen to whole pixels, since
//! fontdue rasterizes at integer offsets only. The faces are monospace, and every advance is a whole
//! number of pixels so columns of text do not jitter between 7 and 8 pixel steps.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use fontdue::{Font, FontSettings, Metrics};
use snafu::Snafu;

use super::canvas::Rgb;

/// IBM Plex Mono in three weights, subset to ASCII plus a few symbols (middle dot, ellipsis, dashes,
/// arrows, multiplication sign, bullet).
const FACES: [&[u8]; Face::COUNT] = [
    include_bytes!("../../assets/fonts/plexmono-regular.ttf"),
    include_bytes!("../../assets/fonts/plexmono-semibold.ttf"),
    include_bytes!("../../assets/fonts/plexmono-bold.ttf"),
];

pub const ELLIPSIS: char = '\u{2026}';

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Face {
    Regular,
    SemiBold,
    Bold,
}

impl Face {
    const COUNT: usize = 3;
    const ALL: [Self; Self::COUNT] = [Self::Regular, Self::SemiBold, Self::Bold];
}

/// How letters are cased when drawn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Case {
    #[default]
    AsWritten,
    /// ASCII letters uppercased, for the small capitals over cards and columns.
    Upper,
}

/// How a run of text looks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Style {
    pub face: Face,
    /// Size in pixels per em.
    pub px: u16,
    pub color: Rgb,
    /// Extra pixels after each glyph.
    pub tracking: f32,
    pub case: Case,
}

#[derive(Debug, Snafu)]
#[snafu(display("parsing embedded face {face:?}: {reason}"))]
pub struct FontError {
    face: Face,
    reason: &'static str,
}

/// A glyph's coverage, one byte per pixel, rows top first.
pub struct Glyph {
    pub metrics: Metrics,
    pub coverage: Vec<u8>,
}

/// Where a glyph's bitmap goes, in whole pixels: left edge relative to the run's origin, top edge
/// relative to the baseline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    pub x: i32,
    pub top: i32,
}

/// The pixels a run of text can colour, relative to its rounded origin and baseline; empty for a
/// run with no ink, such as spaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ink {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Ink {
    const NONE: Self = Self { left: i32::MAX, top: i32::MAX, right: i32::MIN, bottom: i32::MIN };

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.right <= self.left || self.bottom <= self.top
    }

    fn include(&mut self, at: Placement, glyph: &Glyph) {
        let (w, h) = (glyph.metrics.width, glyph.metrics.height);
        if w == 0 || h == 0 {
            return;
        }
        let (w, h) = (i32::try_from(w).unwrap_or(0), i32::try_from(h).unwrap_or(0));
        *self = Self {
            left: self.left.min(at.x),
            top: self.top.min(at.top),
            right: self.right.max(at.x + w),
            bottom: self.bottom.max(at.top + h),
        };
    }
}

/// How far a run advances the pen, and what it draws on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Run {
    pub advance: f32,
    pub ink: Ink,
}

/// Printable ASCII, which almost every glyph drawn is.
const ASCII: core::ops::RangeInclusive<char> = ' '..='~';
const ASCII_COUNT: usize = 95;

/// The glyphs of one face at one size. ASCII is a table lookup; the few symbols go in a map.
struct Strike {
    face: Face,
    px: u16,
    ascii: Vec<Option<Glyph>>,
    other: BTreeMap<char, Glyph>,
}

impl Strike {
    fn new(face: Face, px: u16) -> Self {
        Self { face, px, ascii: (0..ASCII_COUNT).map(|_| None).collect(), other: BTreeMap::new() }
    }

    fn glyph(&mut self, font: &Font, c: char) -> &Glyph {
        let px = f32::from(self.px);
        let make = || {
            let (metrics, coverage) = font.rasterize(c, px);
            Glyph { metrics, coverage }
        };
        if ASCII.contains(&c) {
            self.ascii[c as usize - ' ' as usize].get_or_insert_with(make)
        } else {
            self.other.entry(c).or_insert_with(make)
        }
    }
}

pub struct Fonts {
    faces: Vec<Font>,
    strikes: Vec<Strike>,
}

impl Fonts {
    pub fn load() -> Result<Self, FontError> {
        let faces = Face::ALL
            .into_iter()
            .map(|face| {
                let settings = FontSettings { scale: 32.0, ..FontSettings::default() };
                Font::from_bytes(FACES[face as usize], settings).map_err(|reason| FontError { face, reason })
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { faces, strikes: Vec::new() })
    }

    /// The baseline that centres capitals at `px` in a box from `top` that is `height` tall.
    #[must_use]
    pub fn centred_baseline(&self, px: u16, top: f32, height: f32) -> f32 {
        // Plex Mono's capitals are 0.698 em tall; centring capitals and digits looks right where
        // centring the ascent-to-descent box sits text visibly low.
        let cap = f32::from(px) * 0.7;
        top + f32::midpoint(height, cap)
    }

    fn strike(&mut self, face: Face, px: u16) -> usize {
        if let Some(i) = self.strikes.iter().position(|s| s.face == face && s.px == px) {
            return i;
        }
        self.strikes.push(Strike::new(face, px));
        self.strikes.len() - 1
    }

    pub fn glyph(&mut self, face: Face, px: u16, c: char) -> &Glyph {
        let i = self.strike(face, px);
        self.strikes[i].glyph(&self.faces[face as usize], c)
    }

    /// Call `each` with every glyph of `text` and where it goes; returns the advance of the run.
    pub fn layout(&mut self, text: &str, style: Style, mut each: impl FnMut(Placement, &Glyph)) -> f32 {
        let i = self.strike(style.face, style.px);
        let (strike, font) = (&mut self.strikes[i], &self.faces[style.face as usize]);
        let mut pen = 0.0f32;
        for c in text.chars() {
            let c = if style.case == Case::Upper { c.to_ascii_uppercase() } else { c };
            let glyph = strike.glyph(font, c);
            let m = glyph.metrics;
            let top = -(m.ymin + i32::try_from(m.height).unwrap_or(0));
            each(Placement { x: round(pen) + m.xmin, top }, glyph);
            pen += whole_advance(m) + style.tracking;
        }
        if text.is_empty() { 0.0 } else { pen - style.tracking }
    }

    #[must_use]
    pub fn measure(&mut self, text: &str, style: Style) -> f32 {
        self.layout(text, style, |_, _| ())
    }

    /// The advance and the ink of `text` in one pass.
    pub fn run(&mut self, text: &str, style: Style) -> Run {
        let mut ink = Ink::NONE;
        let advance = self.layout(text, style, |at, glyph| ink.include(at, glyph));
        Run { advance, ink }
    }

    /// The longest prefix of `text` that fits in `max` pixels, and whether it had to be cut. A cut
    /// prefix leaves room for [`ELLIPSIS`] and drops trailing spaces.
    pub fn fit<'t>(&mut self, text: &'t str, style: Style, max: f32) -> (&'t str, bool) {
        if self.measure(text, style) <= max {
            return (text, false);
        }
        let ellipsis = whole_advance(self.glyph(style.face, style.px, ELLIPSIS).metrics);
        let mut width = 0.0;
        let mut end = 0;
        for (i, c) in text.char_indices() {
            let c = if style.case == Case::Upper { c.to_ascii_uppercase() } else { c };
            let advance = whole_advance(self.glyph(style.face, style.px, c).metrics) + style.tracking;
            if width + advance + ellipsis > max {
                break;
            }
            width += advance;
            end = i + c.len_utf8();
        }
        (text[..end].trim_end(), true)
    }
}

/// A glyph's advance in whole pixels.
fn whole_advance(m: Metrics) -> f32 {
    f32::from(i16::try_from(round(m.advance_width)).unwrap_or(0))
}

/// Nearest whole pixel; pens and baselines are never far from zero.
#[must_use]
pub fn round(v: f32) -> i32 {
    if v >= 0.0 { (v + 0.5) as i32 } else { (v - 0.5) as i32 }
}

#[cfg(test)]
mod tests;
