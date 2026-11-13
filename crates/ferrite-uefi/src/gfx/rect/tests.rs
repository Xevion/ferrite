use assert2::check;
use rstest::rstest;

use super::*;

const SCREEN: Rect = Rect::new(0, 0, 1920, 1080);

#[rstest]
#[case::disjoint(Rect::new(0, 0, 10, 10), Rect::new(20, 20, 5, 5), Rect::default())]
#[case::touching(Rect::new(0, 0, 10, 10), Rect::new(10, 0, 5, 5), Rect::default())]
#[case::overlapping(Rect::new(0, 0, 10, 10), Rect::new(5, 5, 10, 10), Rect::new(5, 5, 5, 5))]
#[case::inside(Rect::new(0, 0, 10, 10), Rect::new(2, 3, 4, 5), Rect::new(2, 3, 4, 5))]
fn intersect(#[case] a: Rect, #[case] b: Rect, #[case] expected: Rect) {
    check!(a.intersect(b) == expected);
    check!(b.intersect(a) == expected);
}

#[test]
fn union_ignores_empty() {
    let r = Rect::new(5, 5, 10, 10);
    check!(r.union(Rect::default()) == r);
    check!(Rect::new(100, 100, 0, 7).union(r) == r);
    check!(r.union(Rect::new(20, 0, 5, 5)) == Rect::new(5, 0, 20, 15));
}

#[test]
fn cuts_clamp_to_what_is_left() {
    let mut r = Rect::new(10, 20, 100, 50);
    check!(r.take_top(30) == Rect::new(10, 20, 100, 30));
    check!(r.take_bottom(5) == Rect::new(10, 65, 100, 5));
    check!(r == Rect::new(10, 50, 100, 15));
    check!(r.take_left(40) == Rect::new(10, 50, 40, 15));
    check!(r.take_right(200) == Rect::new(50, 50, 60, 15));
    check!(r.is_empty());
    check!(r.take_top(10).is_empty());
}

#[test]
fn inset_past_the_middle_is_empty() {
    check!(Rect::new(0, 0, 10, 10).inset(2, 3) == Rect::new(2, 3, 6, 4));
    let gone = Rect::new(0, 0, 10, 10).inset(8, 1);
    check!(gone.is_empty());
    check!(gone.x <= 10);
}

#[test]
fn dirty_rects_clip_to_the_screen_and_drop_empties() {
    let mut dirty = DirtyRects::new(SCREEN);
    dirty.add(Rect::new(1900, 1000, 100, 100));
    dirty.add(Rect::new(3000, 0, 10, 10));
    dirty.add(Rect::new(5, 5, 0, 10));
    check!(dirty.as_slice() == [Rect::new(1900, 1000, 20, 80)]);
}

#[test]
fn neighbours_merge_and_distant_rects_stay_apart() {
    let mut dirty = DirtyRects::new(SCREEN);
    // Two stacked bars with a small gap merge; a far corner does not.
    dirty.add(Rect::new(20, 100, 800, 40));
    dirty.add(Rect::new(1500, 900, 100, 20));
    dirty.add(Rect::new(20, 144, 800, 40));
    let mut rects = dirty.as_slice().to_vec();
    rects.sort_by_key(|r| (r.y, r.x));
    check!(rects == [Rect::new(20, 100, 800, 84), Rect::new(1500, 900, 100, 20)]);
}

#[test]
fn a_merge_can_cascade() {
    let mut dirty = DirtyRects::new(SCREEN);
    dirty.add(Rect::new(0, 0, 100, 10));
    dirty.add(Rect::new(0, 20, 100, 10));
    check!(dirty.as_slice().len() == 2);
    // Fills the gap, so all three become one.
    dirty.add(Rect::new(0, 8, 100, 14));
    check!(dirty.as_slice() == [Rect::new(0, 0, 100, 30)]);
}

#[test]
fn too_many_rects_collapse_into_their_bounds() {
    let mut dirty = DirtyRects::new(SCREEN);
    for i in 0..=MAX_RECTS as u32 {
        dirty.add(Rect::new(i * 100, i * 50, 10, 10));
    }
    check!(dirty.as_slice() == [Rect::new(0, 0, MAX_RECTS as u32 * 100 + 10, MAX_RECTS as u32 * 50 + 10)]);
}

/// The rectangles a 64x8 canvas of zeros marks for `rect` once the pixels at `pokes` change.
fn diffed(rect: Rect, pokes: &[(u32, u32)], band: u32) -> Vec<Rect> {
    const W: u32 = 64;
    let canvas = std::vec![0u8; (W * 8 * 4) as usize];
    let before: Vec<u8> = (rect.y..rect.bottom())
        .flat_map(|y| {
            let start = ((y * W + rect.x) * 4) as usize;
            canvas[start..start + rect.w as usize * 4].to_vec()
        })
        .collect();
    let mut after = canvas;
    for &(x, y) in pokes {
        after[((y * W + x) * 4 + 1) as usize] = 0xff;
    }
    let mut dirty = DirtyRects::new(Rect::new(0, 0, W, 8));
    dirty.add_changes(&before, &after, W, rect, band);
    let mut rects = dirty.as_slice().to_vec();
    rects.sort_by_key(|r| (r.x, r.y));
    rects
}

#[test]
fn unchanged_pixels_mark_nothing() {
    check!(diffed(Rect::new(4, 1, 40, 6), &[], 16).is_empty());
    // A change outside the widget is not its business.
    check!(diffed(Rect::new(4, 1, 40, 6), &[(60, 0)], 16).is_empty());
}

#[test]
fn changes_far_apart_get_a_tight_rect_each() {
    let rects = diffed(Rect::new(4, 1, 56, 6), &[(5, 2), (7, 4), (50, 3)], 16);
    check!(rects == [Rect::new(5, 2, 3, 3), Rect::new(50, 3, 1, 1)]);
}

#[test]
fn a_change_across_a_band_edge_is_split_then_merged_back() {
    // Columns 19 and 20 fall in bands 0 and 1 of a widget starting at 4 with 16-pixel bands.
    let rects = diffed(Rect::new(4, 0, 32, 8), &[(19, 5), (20, 5)], 16);
    check!(rects == [Rect::new(19, 5, 2, 1)]);
}
