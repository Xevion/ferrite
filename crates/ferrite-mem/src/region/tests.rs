use std::vec::Vec;

use assert2::check;

use super::*;
use crate::tests::Buf;

fn bufs(words: &[usize]) -> Vec<Buf> {
    words.iter().map(|&w| Buf::new(w)).collect()
}

fn set(bufs: &mut [Buf]) -> Regions<'_> {
    Regions::new(bufs.iter_mut().map(Buf::region))
}

#[test]
fn a_set_trims_each_region_to_whole_groups_and_drops_what_is_left_empty() {
    let mut b = bufs(&[64, 8, 40, 16]);
    let regions = set(&mut b);
    check!(regions.layout() == Layout::new([64, 32, 16]));
    check!(regions.words() == 112);
    check!(
        regions.layout().spans()
            == [Span { start: 0, words: 64 }, Span { start: 64, words: 32 }, Span { start: 96, words: 16 }]
    );
}

#[test]
fn a_range_across_regions_is_one_piece_per_region_in_order() {
    let mut b = bufs(&[64, 48, 128]);
    let mut regions = set(&mut b);
    let addrs: Vec<CpuAddr> = (0..regions.words()).map(|w| regions.addr(w)).collect();
    let got: Vec<(usize, usize, CpuAddr)> = regions.sub(40, 96).map(|(at, r)| (at, r.words(), r.base())).collect();
    check!(got == [(40, 24, addrs[40]), (64, 48, addrs[64]), (112, 24, addrs[112])]);
    let back: Vec<usize> = regions.sub(40, 96).rev().map(|(at, _)| at).collect();
    check!(back == [112, 64, 40]);
    check!(regions.sub(64, 48).count() == 1, "exactly one region is one piece");
    check!(regions.sub(0, 0).count() == 0);
    check!(regions.sub(240, 8).count() == 0, "past the end is nothing");
    let clipped: Vec<(usize, usize)> = regions.sub(232, 64).map(|(at, r)| (at, r.words())).collect();
    check!(clipped == [(232, 8)]);
}

#[test]
fn shares_cut_across_regions_hold_the_words_of_their_span() {
    let mut b = bufs(&[64, 48, 128]);
    let mut regions = set(&mut b);
    let addrs: Vec<CpuAddr> = (0..regions.words()).map(|w| regions.addr(w)).collect();
    let spans = [Span { start: 0, words: 40 }, Span { start: 40, words: 96 }, Span { start: 136, words: 104 }];
    let split = regions.split(&spans);
    for (span, share) in spans.iter().zip(&split) {
        check!(share.words() == span.words);
        check!((0..span.words).all(|w| share.addr(w) == addrs[span.start + w]), "{span:?}");
    }
    check!(split[1].layout().spans().len() == 3, "the middle share touches all three regions");
}

#[test]
fn a_share_leaves_out_the_words_no_span_names() {
    let mut b = bufs(&[64, 64]);
    let mut regions = set(&mut b);
    let split = regions.split(&[Span { start: 16, words: 32 }, Span { start: 72, words: 8 }]);
    check!(split.iter().map(Regions::words).collect::<Vec<_>>() == [32, 8]);
}
