use std::vec::Vec;

use assert2::check;

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

#[test]
fn nothing_is_taken_when_only_the_reserve_is_free() {
    check!(select(&[range(1, 100)], 100 * MIB).is_empty());
    check!(select(&[range(1, 100)], 200 * MIB).is_empty());
}

#[test]
fn pieces_under_the_minimum_are_dropped() {
    let free = [range(1, 100), range(500, 1), Range::new(600 * 256, 255)];
    check!(spans(&select(&free, 0)) == [(1, 100), (500, 1)]);
}

#[test]
fn touching_and_overlapping_ranges_become_one() {
    let free = [range(48, 16), range(16, 16), range(32, 16), range(60, 8)];
    check!(spans(&select(&free, 0)) == [(16, 52)]);
}

#[test]
fn subtracting_cuts_holes_out_of_the_middle_and_the_ends() {
    let free = [range(10, 100), range(200, 10)];
    let hole = [range(20, 10), range(100, 20), range(205, 1)];
    check!(spans(&subtract(&free, &hole)) == [(10, 10), (30, 70), (200, 5), (206, 4)]);
    check!(subtract(&free, &[range(0, 300)]).is_empty());
    check!(spans(&subtract(&free, &[])) == [(10, 100), (200, 10)]);
}

#[test]
fn overlapping_holes_are_cut_once() {
    check!(spans(&subtract(&[range(0, 100)], &[range(40, 20), range(50, 20), range(45, 2)])) == [(0, 40), (70, 30)]);
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
