//! The embedded faces, a cache of rasterized glyphs, and text measuring and fitting.
//!
//! Every glyph's position is decided in [`Fonts::layout`]; it rounds the pen to whole pixels, since
//! fontdue rasterizes at integer offsets only.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use fontdue::{Font, FontSettings, Metrics};
use snafu::Snafu;

use super::canvas::Rgb;

/// Inter for words, `JetBrains Mono` for numbers and addresses so changing digits keep their width.
/// Both are subset to ASCII plus a few symbols (middle dot, ellipsis, arrows, bullet).
const FACES: [&[u8]; Face::COUNT] = [
    include_bytes!("../../assets/fonts/inter-regular.ttf"),
    include_bytes!("../../assets/fonts/inter-semibold.ttf"),
    include_bytes!("../../assets/fonts/inter-bold.ttf"),
    include_bytes!("../../assets/fonts/jetbrainsmono-regular.ttf"),
    include_bytes!("../../assets/fonts/jetbrainsmono-bold.ttf"),
];

pub const ELLIPSIS: char = '\u{2026}';

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Face {
    Regular,
    SemiBold,
    Bold,
    Mono,
    MonoBold,
}

impl Face {
    const COUNT: usize = 5;
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

pub struct Fonts {
    faces: Vec<Font>,
    cache: BTreeMap<(Face, u16, char), Glyph>,
}

impl Fonts {
    pub fn load() -> Result<Self, FontError> {
        let faces = [Face::Regular, Face::SemiBold, Face::Bold, Face::Mono, Face::MonoBold]
            .into_iter()
            .map(|face| {
                let settings = FontSettings { scale: 32.0, ..FontSettings::default() };
                Font::from_bytes(FACES[face as usize], settings).map_err(|reason| FontError { face, reason })
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { faces, cache: BTreeMap::new() })
    }

    /// The baseline that centres capitals at `px` in a box from `top` that is `height` tall.
    #[must_use]
    pub fn centred_baseline(&self, px: u16, top: f32, height: f32) -> f32 {
        // Cap height is about 0.73 em in Inter and JetBrains Mono; centring capitals and digits
        // looks right where centring the ascent-to-descent box sits text visibly low.
        let cap = f32::from(px) * 0.73;
        top + f32::midpoint(height, cap)
    }

    pub fn glyph(&mut self, face: Face, px: u16, c: char) -> &Glyph {
        let font = &self.faces[face as usize];
        self.cache.entry((face, px, c)).or_insert_with(|| {
            let (metrics, coverage) = font.rasterize(c, f32::from(px));
            Glyph { metrics, coverage }
        })
    }

    /// Call `each` with every glyph of `text` and where it goes; returns the advance of the run.
    pub fn layout(&mut self, text: &str, style: Style, mut each: impl FnMut(Placement, &Glyph)) -> f32 {
        let mut pen = 0.0f32;
        for c in text.chars() {
            let glyph = self.glyph(style.face, style.px, c);
            let m = glyph.metrics;
            let top = -(m.ymin + i32::try_from(m.height).unwrap_or(0));
            each(Placement { x: round(pen) + m.xmin, top }, glyph);
            pen += m.advance_width + style.tracking;
        }
        if text.is_empty() { 0.0 } else { pen - style.tracking }
    }

    #[must_use]
    pub fn measure(&mut self, text: &str, style: Style) -> f32 {
        self.layout(text, style, |_, _| ())
    }

    /// The longest prefix of `text` that fits in `max` pixels, and whether it had to be cut. A cut
    /// prefix leaves room for [`ELLIPSIS`] and drops trailing spaces.
    pub fn fit<'t>(&mut self, text: &'t str, style: Style, max: f32) -> (&'t str, bool) {
        if self.measure(text, style) <= max {
            return (text, false);
        }
        let ellipsis = self.glyph(style.face, style.px, ELLIPSIS).metrics.advance_width;
        let mut width = 0.0;
        let mut end = 0;
        for (i, c) in text.char_indices() {
            let advance = self.glyph(style.face, style.px, c).metrics.advance_width + style.tracking;
            if width + advance + ellipsis > max {
                break;
            }
            width += advance;
            end = i + c.len_utf8();
        }
        (text[..end].trim_end(), true)
    }
}

/// Nearest whole pixel; pens and baselines are never far from zero.
#[must_use]
pub fn round(v: f32) -> i32 {
    if v >= 0.0 { (v + 0.5) as i32 } else { (v - 0.5) as i32 }
}

#[cfg(test)]
mod tests;
