//! Where every card and widget of the dashboard goes, scaled from a 1360x860 design to the screen.

use crate::gfx::{Rect, px, whole_px};

/// The size the dashboard's measures are given for.
const DESIGN: (f32, f32) = (1360.0, 860.0);

/// Text sizes in pixels, scaled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Type {
    /// Small capitals over cards and columns.
    pub label: u16,
    pub body: u16,
    pub small: u16,
    pub mono: u16,
    pub title: u16,
    pub big: u16,
}

/// Every rectangle of one screen size. Cards are the static frames; the other rectangles are
/// widgets inside them, each redrawn on its own when what it shows changes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Layout {
    pub scale: f32,
    pub text: Type,
    pub radius: f32,
    /// Padding inside a card.
    pub pad: u32,
    /// Height of a card's title strip, and of a table row.
    pub title_h: u32,
    pub row_h: u32,

    pub header: Rect,
    pub brand: Rect,
    pub clock: Rect,

    pub progress: Rect,
    pub pass: Rect,
    pub test: Rect,

    pub tests_card: Rect,
    pub tests: Rect,
    pub faults_card: Rect,
    pub faults: Rect,
    pub net_card: Rect,
    pub net: Rect,
    pub passes_card: Rect,
    pub passes: Rect,
    pub cpus_card: Rect,
    pub cpus: Rect,

    pub footer: Rect,
}

/// The design-to-screen factor: the smaller of the two axes, so everything fits.
#[must_use]
pub fn scale(width: u32, height: u32) -> f32 {
    let s = (px(width) / DESIGN.0).min(px(height) / DESIGN.1);
    s.clamp(0.75, 4.0)
}

fn text_px(v: f32) -> u16 {
    u16::try_from(crate::gfx::text::round(v)).unwrap_or(u16::MAX).max(6)
}

impl Layout {
    /// `test_rows` is how many tests the preset runs; with `faulted` the fault log gets more room.
    #[must_use]
    pub fn new(width: u32, height: u32, test_rows: usize, faulted: bool) -> Self {
        let s = scale(width, height);
        let m = |v: f32| whole_px(v * s);
        let (margin, gap, pad) = (m(18.0), m(12.0), m(16.0));
        let (title_h, row_h) = (m(26.0), m(22.0));
        let text = Type {
            label: text_px(10.5 * s),
            body: text_px(13.5 * s),
            small: text_px(12.0 * s),
            mono: text_px(12.5 * s),
            title: text_px(16.0 * s),
            big: text_px(26.0 * s),
        };

        let mut area = Rect::new(0, 0, width, height).inset(margin, margin);
        let header = area.take_top(m(56.0));
        area.take_top(gap);
        let footer = area.take_bottom(m(28.0));
        area.take_bottom(gap / 2);
        let progress = area.take_top(m(118.0));
        area.take_top(gap);

        let mut head = header.inset(pad, 0);
        let clock = head.take_right(head.w / 2);
        let brand = head;

        let mut bars = progress.inset(pad, pad);

        let left_w = whole_px(px(area.w.saturating_sub(gap)) * 0.6);
        let mut left = area.take_left(left_w);
        area.take_left(gap);
        let mut right = area;

        let body_of = |card: Rect| {
            let mut body = card.inset(pad, pad);
            body.take_top(title_h);
            body
        };
        let fault_rows: u32 = if faulted { 6 } else { 2 };
        let faults_min = 2 * pad + title_h + row_h * (fault_rows + 1);
        let rows = u32::try_from(test_rows).unwrap_or(u32::MAX);
        let tests_need = 2 * pad + title_h + row_h * rows.saturating_add(1);
        let tests_card = left.take_top(tests_need.min(left.h.saturating_sub(faults_min + gap)));
        left.take_top(gap);
        let faults_card = left;

        let net_card = right.take_top(m(176.0));
        right.take_top(gap);
        let cpus_card = right.take_bottom(m(132.0));
        right.take_bottom(gap);
        let passes_card = right;

        Self {
            scale: s,
            text,
            radius: 10.0 * s,
            pad,
            title_h,
            row_h,
            header,
            brand,
            clock,
            progress,
            pass: bars.take_top(bars.h / 2),
            test: bars,
            tests_card,
            tests: body_of(tests_card),
            faults_card,
            faults: body_of(faults_card),
            net_card,
            net: body_of(net_card),
            passes_card,
            passes: body_of(passes_card),
            cpus_card,
            cpus: body_of(cpus_card),
            footer,
        }
    }

    /// `v` design pixels on this screen.
    #[must_use]
    pub fn m(&self, v: f32) -> f32 {
        v * self.scale
    }
}

/// The first of `total` rows to show when `visible` fit, keeping row `current` in view with as
/// many rows after it as before.
#[must_use]
pub fn window(total: usize, visible: usize, current: usize) -> usize {
    if total <= visible {
        return 0;
    }
    current.saturating_sub(visible / 2).min(total - visible)
}

/// The side of square tiles, and how many go in a row, for `n` tiles in `w` by `h` with `gap`
/// between them: the largest side up to `max` that fits, down to `min` (rows past `h` then clip).
#[must_use]
pub fn tile_grid(n: u32, w: u32, h: u32, gap: u32, max: u32, min: u32) -> (u32, u32) {
    let n = n.max(1);
    let fits = |side: u32| {
        let cols = ((w + gap) / (side + gap)).max(1);
        let rows = n.div_ceil(cols);
        (cols, rows * (side + gap) - gap <= h)
    };
    let mut side = max.max(min);
    while side > min {
        if let (cols, true) = fits(side) {
            return (side, cols.min(n));
        }
        side -= 1;
    }
    (side, fits(side).0.min(n))
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::fits(10, 12, 7, 0)]
    #[case::near_the_top(16, 8, 1, 0)]
    #[case::middle(16, 8, 9, 5)]
    #[case::near_the_end(16, 8, 15, 8)]
    fn window_keeps_the_current_row_in_view(
        #[case] total: usize,
        #[case] visible: usize,
        #[case] current: usize,
        #[case] first: usize,
    ) {
        let start = window(total, visible, current);
        check!(start == first);
        check!((start..start + visible.min(total)).contains(&current));
    }

    #[test]
    fn tiles_shrink_until_every_row_fits() {
        // Eight tiles fit one row at full size.
        check!(tile_grid(8, 400, 40, 6, 36, 10) == (36, 8));
        // Sixty-four need three rows of 22 at most in 80 pixels.
        let (side, cols) = tile_grid(64, 600, 80, 4, 36, 10);
        assert!(side < 36);
        let rows = 64u32.div_ceil(cols);
        check!(rows * (side + 4) - 4 <= 80);
        check!(cols * (side + 4) - 4 <= 600);
        // And a little bigger would not have fit.
        let bigger_cols = (600 + 4) / (side + 1 + 4);
        check!(64u32.div_ceil(bigger_cols) * (side + 5) - 4 > 80);
    }

    #[test]
    fn tiles_stop_at_the_minimum() {
        check!(tile_grid(1000, 100, 20, 2, 30, 8).0 == 8);
    }

    #[rstest]
    #[case::ovmf_default(1280, 800)]
    #[case::full_hd(1920, 1080)]
    #[case::uhd(3840, 2160)]
    #[case::narrow(1024, 768)]
    fn cards_stay_on_screen_and_apart(#[case] w: u32, #[case] h: u32) {
        for faulted in [false, true] {
            let l = Layout::new(w, h, 16, faulted);
            let screen = Rect::new(0, 0, w, h);
            let cards = [l.header, l.progress, l.tests_card, l.faults_card, l.net_card, l.passes_card, l.cpus_card];
            for (i, a) in cards.iter().enumerate() {
                check!(a.intersect(screen) == *a);
                check!(!a.is_empty(), "card {i} is empty at {w}x{h}");
                for b in &cards[i + 1..] {
                    check!(a.intersect(*b).is_empty());
                }
            }
            check!(l.faults.h >= l.row_h * 3, "fault log too short at {w}x{h}");
        }
    }
}
