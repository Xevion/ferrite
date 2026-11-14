use std::vec::Vec;

use assert2::check;
use rstest::rstest;

use super::*;

fn reference_move(buf: &[u8], dst: usize, src: usize, n: usize) -> Vec<u8> {
    let mut out = buf.to_vec();
    out.copy_within(src..src + n, dst);
    out
}

#[rstest]
#[case::dst_overlaps_above(5, 0, 20)]
#[case::dst_overlaps_below(0, 5, 20)]
#[case::one_byte_apart_above(1, 0, 31)]
#[case::one_byte_apart_below(0, 1, 31)]
#[case::same_range(4, 4, 16)]
#[case::disjoint(0, 16, 16)]
#[case::empty(3, 9, 0)]
fn move_matches_copy_within(#[case] dst: usize, #[case] src: usize, #[case] n: usize) {
    let buf: Vec<u8> = (0..32).collect();
    let want = reference_move(&buf, dst, src, n);
    let mut got = buf;
    let base = got.as_mut_ptr();
    // SAFETY: both ranges lie inside the 32-byte buffer.
    unsafe { move_bytes(base.add(dst), base.add(src), n) };
    check!(got == want);
}

#[test]
fn fill_stores_only_the_range() {
    let mut buf = [0_u8; 16];
    // SAFETY: bytes 4..12 of the buffer.
    unsafe { fill(buf.as_mut_ptr().add(4), 0xa5, 8) };
    check!(buf == [0, 0, 0, 0, 0xa5, 0xa5, 0xa5, 0xa5, 0xa5, 0xa5, 0xa5, 0xa5, 0, 0, 0, 0]);
}

#[rstest]
#[case::equal(b"abcd", b"abcd", 0)]
#[case::bytes_compare_unsigned(b"ab\x80d", b"ab\x01d", 0x7f)]
#[case::less(b"ab\x01d", b"ab\x80d", -0x7f)]
#[case::first_difference_wins(b"\x00\xff", b"\x01\x00", -1)]
#[case::equal_past_a_word(b"0123456789abcdefXY", b"0123456789abcdefXY", 0)]
#[case::first_difference_in_a_word_wins(b"0123\x80\x00670123", b"0123\x01\xff670123", 0x7f)]
#[case::difference_in_the_tail(b"01234567ab\x02", b"01234567ab\x05", -3)]
fn compare_signs(#[case] a: &[u8], #[case] b: &[u8], #[case] want: c_int) {
    let n = a.len().min(b.len());
    // SAFETY: `n` is within both slices.
    let got = unsafe { compare(a.as_ptr(), b.as_ptr(), n) };
    check!(got == want);
}

#[rstest]
#[case::empty(b"\0", 0)]
#[case::stops_at_the_first_nul(b"ab\0cd\0", 2)]
fn length_counts_to_the_nul(#[case] s: &[u8], #[case] want: usize) {
    // SAFETY: each case holds a NUL.
    check!(unsafe { length(s.as_ptr()) } == want);
}
