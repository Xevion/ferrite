use std::vec;
use std::vec::Vec;

use assert2::check;
use rstest::rstest;

use super::*;

const MIB: u64 = 1 << 20;

/// `mib` MiB from `start` MiB.
fn range(start: u64, mib: u64) -> Range {
    Range::new(start * MIB, (mib * MIB / PAGE_SIZE as u64) as usize)
}

fn spans(ranges: &[Range]) -> Vec<(u64, u64)> {
    ranges.iter().map(|r| (r.start.get() / MIB, r.bytes() / MIB)).collect()
}

#[test]
fn the_lowest_memory_is_never_taken() {
    let free = [Range::new(0, 160), range(0, 8), range(2, 64)];
    check!(spans(&select(&free, 0)) == [(1, 65)]);
    check!(eligible_bytes(&free) == 65 * MIB);
}

#[test]
fn the_lowest_reserve_is_left_and_the_rest_taken_largest_first() {
    let free = [range(4096, 2048), range(1, 100), range(300, 50)];
    let taken = select(&free, 120 * MIB);
    check!(spans(&taken) == [(4096, 2048), (320, 30)]);
    check!(eligible_bytes(&free) - taken.iter().map(|r| r.bytes()).sum::<u64>() == 120 * MIB);
}

#[rstest]
#[case::pieces_under_the_minimum_are_dropped(
    vec![range(1, 100), range(500, 1), Range::new(600 * 256, 255)],
    0,
    vec![(1, 100), (500, 1)]
)]
#[case::touching_and_overlapping_ranges_become_one(
    vec![range(48, 16), range(16, 16), range(32, 16), range(60, 8)],
    0,
    vec![(16, 52)]
)]
#[case::only_the_reserve_is_free(vec![range(1, 100)], 100 * MIB, vec![])]
#[case::the_reserve_exceeds_what_is_free(vec![range(1, 100)], 200 * MIB, vec![])]
fn select_takes(#[case] free: Vec<Range>, #[case] reserve: u64, #[case] expected: Vec<(u64, u64)>) {
    check!(spans(&select(&free, reserve)) == expected);
}

#[rstest]
#[case::holes_in_the_middle_and_the_ends(
    vec![range(10, 100), range(200, 10)],
    vec![range(20, 10), range(100, 20), range(205, 1)],
    vec![(10, 10), (30, 70), (200, 5), (206, 4)]
)]
#[case::a_hole_covering_everything(vec![range(10, 100), range(200, 10)], vec![range(0, 300)], vec![])]
#[case::no_holes(vec![range(10, 100), range(200, 10)], vec![], vec![(10, 100), (200, 10)])]
#[case::overlapping_holes_are_cut_once(
    vec![range(0, 100)],
    vec![range(40, 20), range(50, 20), range(45, 2)],
    vec![(0, 40), (70, 30)]
)]
fn subtracting(#[case] free: Vec<Range>, #[case] holes: Vec<Range>, #[case] expected: Vec<(u64, u64)>) {
    check!(spans(&subtract(&free, &holes)) == expected);
}

#[test]
fn intersecting_keeps_only_what_both_have() {
    let lent = [range(100, 50), range(300, 20)];
    let free = [range(120, 100), range(310, 5)];
    check!(spans(&intersect(&lent, &free)) == [(120, 30), (310, 5)]);
}

#[test]
fn held_pages_are_summed_by_kind_inside_the_zone() {
    let entries = [(1_u32, range(0, 4)), (2, range(2, 10)), (1, range(20, 10)), (2, range(60, 40)), (3, range(40, 1))];
    check!(held_below(entries, 64 * MIB) == [(1, 13 * MIB), (2, 14 * MIB), (3, MIB)]);
}
