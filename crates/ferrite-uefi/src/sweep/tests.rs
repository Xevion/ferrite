use alloc::vec;
use alloc::vec::Vec;

use assert2::{assert, check};

use super::*;

fn sweep_of_words(words: usize) -> Sweep {
    Sweep::with_buffer(vec![0; words])
}

/// Step with `chunk_bytes` until `passes` full passes are done, recording every status.
fn run_passes(sweep: &mut Sweep, chunk_bytes: usize, passes: u64) -> Vec<SweepStatus> {
    let mut seen = vec![sweep.status()];
    while sweep.status().passes < passes {
        sweep.step(chunk_bytes);
        seen.push(sweep.status());
    }
    seen
}

#[test]
fn phases_and_patterns_cycle_over_passes() {
    let mut sweep = sweep_of_words(10);
    let seen = run_passes(&mut sweep, 3 * WORD, 2);
    let mut transitions: Vec<(u8, Phase, u64)> = seen.iter().map(|s| (s.pattern, s.phase, s.passes)).collect();
    transitions.dedup();
    let one_pass = |pass| PATTERNS.into_iter().flat_map(move |p| [(p, Phase::Write, pass), (p, Phase::Verify, pass)]);
    let expected: Vec<_> = one_pass(0).chain(one_pass(1)).chain([(0x00, Phase::Write, 2)]).collect();
    check!(transitions == expected);
}

#[test]
fn percent_is_monotonic_within_a_pass_and_never_100() {
    let mut sweep = sweep_of_words(37);
    let seen = run_passes(&mut sweep, 5 * WORD, 3);
    for pair in seen.windows(2) {
        let [before, after] = pair else { unreachable!() };
        if before.passes == after.passes {
            check!(after.percent >= before.percent);
        } else {
            check!(after.percent == 0);
        }
    }
    check!(seen.iter().all(|s| s.percent <= 99));
    check!(seen.iter().any(|s| s.percent >= 90));
}

#[test]
fn chunk_smaller_than_a_word_still_progresses() {
    let mut sweep = sweep_of_words(4);
    sweep.step(1);
    check!(sweep.offset == 1);
}

#[test]
fn corrupted_word_is_reported_with_its_address() {
    let mut sweep = sweep_of_words(8);
    sweep.step(8 * WORD);
    sweep.step(8 * WORD);
    sweep.step(8 * WORD);
    let status = sweep.status();
    assert!(status.pattern == 0xFF);
    assert!(status.phase == Phase::Verify);

    let actual = 0xFFFF_FFFF_FFEF_FFFF;
    sweep.buf[3] = actual;
    sweep.step(5 * WORD);
    sweep.step(5 * WORD);

    let status = sweep.status();
    check!(status.faults == 1);
    let expected = Fault { addr: CpuAddr::of(&raw const sweep.buf[3]), expected: u64::MAX, actual };
    check!(status.first_fault == Some(expected));
    check!(status.phase == Phase::Write);
}

#[test]
fn empty_buffer_has_no_work() {
    let mut sweep = sweep_of_words(0);
    sweep.step(4096);
    let status = sweep.status();
    check!(!sweep.has_work());
    check!(status.percent == 0);
    check!(status.passes == 0);
    check!(status.bytes == 0);
}

#[test]
fn allocation_halves_until_it_fits() {
    let mut tried = Vec::new();
    let buf = alloc_largest(64 << 10, 4 << 10, |bytes| {
        tried.push(bytes);
        (bytes <= 10 << 10).then(|| vec![0; bytes / WORD])
    });
    check!(tried == [64 << 10, 32 << 10, 16 << 10, 8 << 10]);
    check!(buf.len() * WORD == 8 << 10);
}

#[test]
fn allocation_gives_up_below_the_minimum() {
    let mut tried = Vec::new();
    let buf = alloc_largest(24 << 10, 8 << 10, |bytes| {
        tried.push(bytes);
        None
    });
    check!(tried == [24 << 10, 12 << 10, 8 << 10]);
    check!(buf.is_empty());
}
