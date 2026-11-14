//! Where every part of the dashboard goes. Measures are given for a 1280x800 screen, or for
//! 1024x768 below 1200 pixels of width, and scaled up from there.

use crate::gfx::{Rect, px, whole_px};

/// Screens narrower than this get the compact layout.
const COMPACT_BELOW: u32 = 1200;
const WIDE: (f32, f32) = (1280.0, 800.0);
const COMPACT: (f32, f32) = (1024.0, 768.0);

/// Text sizes in pixels, scaled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Type {
    /// Body text.
    pub base: u16,
    /// Section labels and column heads.
    pub label: u16,
    pub small: u16,
    pub title: u16,
    /// The error count.
    pub big: u16,
}

/// A titled section: the band with its label and rule, and the body below.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Section {
    pub label: Rect,
    pub body: Rect,
}

/// Rows of the SYSTEM grid and of the NETWORK list.
pub const SYSTEM_ROWS: u32 = 3;
pub const NET_ROWS: u32 = 4;
/// Columns of the memory map, and its rows.
pub const MAP_COLS: u32 = 16;
pub const MAP_ROWS: u32 = 8;

/// Every rectangle of one screen size. The bars and sections are static, drawn on a full redraw;
/// the other rectangles are widgets, each redrawn on its own when what it shows changes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Layout {
    pub scale: f32,
    pub text: Type,
    /// The height of a line of text, and of a table row with its padding.
    pub row_h: u32,
    pub pitch: u32,
    /// Width of a label column, and of the right-aligned figure after a progress bar.
    pub lab: u32,
    pub pw: u32,
    /// Widths of the tests table's number, status, errors and time columns.
    pub cols: [u32; 4],

    pub header: Rect,
    pub clock: Rect,
    pub tabs: Rect,
    pub log_rule: Rect,
    pub log: Rect,
    pub footer: Rect,

    pub system: Section,
    pub progress: Section,
    pub tests: Section,
    pub map: Section,
    pub net: Section,
    pub errors: Section,

    pub pass: Rect,
    pub test: Rect,
    pub detail_rule: Rect,
    pub detail: Rect,
    pub tests_head: Rect,
    pub tests_rows: Rect,
}

/// The design-to-screen factor: the smaller of the two axes, so everything fits.
#[must_use]
pub fn scale(width: u32, height: u32) -> f32 {
    let (dw, dh) = if width < COMPACT_BELOW { COMPACT } else { WIDE };
    (px(width) / dw).min(px(height) / dh).clamp(0.75, 4.0)
}

fn text_px(v: f32) -> u16 {
    u16::try_from(crate::gfx::text::round(v)).unwrap_or(u16::MAX).max(6)
}

impl Layout {
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        let compact = width < COMPACT_BELOW;
        let s = scale(width, height);
        let m = |v: f32| u32::try_from(crate::gfx::text::round(v * s)).unwrap_or(0);
        let sizes = Type {
            base: text_px(13.0 * s),
            label: text_px(12.0 * s),
            small: text_px(11.0 * s),
            title: text_px(16.0 * s),
            big: text_px(56.0 * s),
        };
        let row_h = m(19.0);
        let pitch = row_h + m(2.0);
        let (gap, section_gap) = (m(12.0), m(18.0));
        // The band over a section's body: its label, the rule under it, and the space after.
        let (label_h, label_to_body) = (m(21.0), m(29.0));

        let mut area = Rect::new(0, 0, width, height);
        let header = area.take_top(m(44.0));
        let tabs = area.take_top(m(34.0));
        let footer = area.take_bottom(m(32.0));
        let log_strip = area.take_bottom(m(26.0));
        let log_rule = Rect::new(log_strip.x, log_strip.y, log_strip.w, 1);
        let log = Rect::new(log_strip.x, log_strip.y + 1, log_strip.w, log_strip.h.saturating_sub(1));

        let (pad_x, pad_top, pad_bottom) = (m(12.0), m(16.0), m(8.0));
        let main = Rect::new(
            area.x + pad_x,
            area.y + pad_top,
            area.w.saturating_sub(2 * pad_x),
            area.h.saturating_sub(pad_top + pad_bottom),
        );
        let left_w = whole_px(px(main.w.saturating_sub(gap)) * if compact { 0.591 } else { 0.62 });
        let (left_x, right_x) = (main.x, main.x + left_w + gap);
        let right_w = main.w.saturating_sub(left_w + gap);

        let section = |x: u32, y: u32, w: u32, body_h: u32| Section {
            label: Rect::new(x, y, w, label_h),
            body: Rect::new(x, y + label_to_body, w, body_h),
        };

        let system = section(left_x, main.y, left_w, SYSTEM_ROWS * pitch - m(2.0));
        let (lab, pw) = if compact { (m(84.0), m(104.0)) } else { (m(92.0), m(120.0)) };

        let progress_y = system.body.bottom() + section_gap;
        let bars_y = progress_y + label_to_body;
        let bar_gap = m(8.0);
        let pass = Rect::new(left_x, bars_y, left_w, 2 * row_h + bar_gap);
        let test = Rect::new(left_x, pass.bottom() + bar_gap, left_w, row_h);
        let detail_rule = Rect::new(left_x, test.bottom() + bar_gap, left_w, 1);
        let detail = Rect::new(left_x, detail_rule.bottom() + m(8.0), left_w, 2 * row_h + m(2.0));
        let progress = Section {
            label: Rect::new(left_x, progress_y, left_w, label_h),
            body: Rect::new(left_x, bars_y, left_w, detail.bottom() - bars_y),
        };

        let tests_y = progress.body.bottom() + section_gap;
        let tests_body_h = main.bottom().saturating_sub(tests_y + label_to_body);
        let tests = section(left_x, tests_y, left_w, tests_body_h);
        let head_h = m(21.0).min(tests.body.h);
        let mut rows = tests.body;
        let tests_head = rows.take_top(head_h);

        let cell_h = m(20.0);
        let map_head = row_h + m(6.0);
        let grid_h = MAP_ROWS * cell_h + (MAP_ROWS - 1) * m(3.0);
        let map = section(right_x, main.y, right_w, map_head + grid_h + m(10.0) + m(16.0));
        let net_y = map.body.bottom() + section_gap;
        let net = section(right_x, net_y, right_w, NET_ROWS * pitch - m(2.0));
        let errors_y = net.body.bottom() + section_gap;
        let errors = section(right_x, errors_y, right_w, main.bottom().saturating_sub(errors_y + label_to_body));

        let cols = if compact { [28.0, 84.0, 52.0, 52.0] } else { [36.0, 96.0, 64.0, 72.0] }.map(m);
        let clock = Rect::new(header.x + header.w / 2, header.y, header.w / 2 - m(20.0), header.h);

        Self {
            scale: s,
            text: sizes,
            row_h,
            pitch,
            lab,
            pw,
            cols,
            header,
            clock,
            tabs,
            log_rule,
            log,
            footer,
            system,
            progress,
            tests,
            map,
            net,
            errors,
            pass,
            test,
            detail_rule,
            detail,
            tests_head,
            tests_rows: rows,
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

#[cfg(test)]
mod tests {
    use assert2::check;
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

    #[rstest]
    #[case::minimum(768, 576)]
    #[case::narrow(1024, 768)]
    #[case::ovmf_default(1280, 800)]
    #[case::full_hd(1920, 1080)]
    #[case::uhd(3840, 2160)]
    fn sections_stay_on_screen_and_apart(#[case] w: u32, #[case] h: u32) {
        let l = Layout::new(w, h);
        let screen = Rect::new(0, 0, w, h);
        let sections = [l.system, l.progress, l.tests, l.map, l.net, l.errors];
        let parts = [
            l.header,
            l.tabs,
            l.log,
            l.footer,
            l.system.body,
            l.progress.body,
            l.tests.body,
            l.map.body,
            l.net.body,
            l.errors.body,
        ];
        for (i, a) in parts.iter().enumerate() {
            check!(a.intersect(screen) == *a, "part {i} leaves the screen at {w}x{h}");
            check!(!a.is_empty(), "part {i} is empty at {w}x{h}");
            for b in &parts[i + 1..] {
                check!(a.intersect(*b).is_empty(), "parts overlap at {w}x{h}");
            }
        }
        for s in sections {
            check!(s.label.bottom() <= s.body.y);
        }
        check!(l.tests_rows.h >= 6 * l.pitch, "tests table too short at {w}x{h}");
        check!(l.errors.body.h >= u32::from(l.text.big) + 4 * l.pitch, "errors too short at {w}x{h}");
    }
}
