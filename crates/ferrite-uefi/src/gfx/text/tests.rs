use assert2::{assert, check};
use rstest::rstest;

use super::*;

const BODY: Style = Style { face: Face::Regular, px: 16, color: Rgb::hex(0x00ff_ffff), tracking: 0.0 };

fn fonts() -> Fonts {
    Fonts::load().expect("embedded faces parse")
}

#[test]
fn every_face_has_the_symbols_the_dashboard_draws() {
    let fonts = fonts();
    for face in [Face::Regular, Face::SemiBold, Face::Bold, Face::Mono, Face::MonoBold] {
        for c in [ELLIPSIS, '\u{b7}', 'g', '0'] {
            check!(fonts.faces[face as usize].has_glyph(c), "{face:?} lacks {c:?}");
        }
    }
}

#[test]
fn text_that_fits_is_left_whole() {
    let mut fonts = fonts();
    let width = fonts.measure("moving inversions", BODY);
    check!(fonts.fit("moving inversions", BODY, width) == ("moving inversions", false));
}

#[rstest]
#[case::mid_word(60.0)]
#[case::after_a_space(75.0)]
#[case::one_letter(14.0)]
fn cut_text_leaves_room_for_the_ellipsis(#[case] max: f32) {
    let mut fonts = fonts();
    let text = "moving inversions, 64-bit shifting";
    let (prefix, cut) = fonts.fit(text, BODY, max);
    assert!(cut);
    check!(text.starts_with(prefix));
    check!(!prefix.ends_with(' '));
    let ellipsis = fonts.measure("\u{2026}", BODY);
    check!(fonts.measure(prefix, BODY) + ellipsis <= max + 0.01);
}

#[test]
fn no_room_at_all_gives_nothing() {
    let mut fonts = fonts();
    check!(fonts.fit("checkerboard", BODY, 0.0) == ("", true));
}

#[test]
fn tracking_widens_between_glyphs_only() {
    let mut fonts = fonts();
    let plain = fonts.measure("PASS", BODY);
    let tracked = fonts.measure("PASS", Style { tracking: 2.0, ..BODY });
    check!((tracked - plain - 6.0).abs() < 0.01);
    check!(fonts.measure("", Style { tracking: 2.0, ..BODY }).abs() < f32::EPSILON);
}
