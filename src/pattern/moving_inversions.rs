//! Moving inversions: fill the buffer with a seed word, then sweep it forward
//! writing the inverse and backward writing the seed again, reading each cell
//! against its expected value before overwriting it.
//!
//! The directional read-then-write is what exposes transition faults (a cell
//! that will not flip) and coupling between adjacent words that a bulk
//! fill-then-verify misses. Each seed runs four phases: fill, forward-invert,
//! backward-invert, and a final verify that the buffer returned to the seed.
//!
//! Like the march sequence, the sweep is strictly sequential: splitting the
//! buffer across threads would break the traversal order the fault coverage
//! depends on, so the `parallel` flag is accepted only for signature parity.

use std::ptr;

use crate::ops::CHUNK_WORDS as REPORT_CHUNK;
use crate::shutdown;
use crate::{Failure, FailureBudget};

/// Sweep traversal order.
#[derive(Clone, Copy, PartialEq)]
enum Dir {
    Up,
    Down,
}

use Dir::{Down, Up};

/// One directional pass over the buffer. `expect`, when set, is read and
/// compared at each cell before it is touched; `write`, when set, is stored
/// afterwards.
#[derive(Clone, Copy)]
struct Phase {
    dir: Dir,
    expect: Option<u64>,
    write: Option<u64>,
}

/// Seed words. Each seed `p` also exercises `!p` via the invert passes, so
/// `0x0` covers all-zero/all-one and `0xAAAA…` covers the two checkerboards.
const SEEDS: &[u64] = &[0x0000_0000_0000_0000, 0xAAAA_AAAA_AAAA_AAAA];

/// Phases run per seed: fill, forward-invert, backward-invert, final verify.
const PHASES_PER_SEED: u64 = 4;

/// Total sub-passes across every seed.
pub(super) const SUB_PASSES: u64 = SEEDS.len() as u64 * PHASES_PER_SEED;

/// Sweep `buf` once in the phase's direction, reading each cell against
/// `phase.expect` (when set) before overwriting it with `phase.write` (when set),
/// pushing a [`Failure`] on every mismatch.
///
/// Returns `true` if the sweep should stop early -- a quit was requested or the
/// shared [`FailureBudget`] was exhausted. Budget, activity, and cancellation
/// are checked every [`REPORT_CHUNK`] cells so a wholly-bad sweep bails out
/// without materializing one record per failing word.
fn sweep(
    buf: &mut [u64],
    phase: Phase,
    base_addr: usize,
    failures: &mut Vec<Failure>,
    budget: &FailureBudget,
    on_activity: &(dyn Fn(f64) + Sync),
) -> bool {
    let len = buf.len();
    if len == 0 {
        return false;
    }
    let base = buf.as_mut_ptr();

    let apply = |i: usize, failures: &mut Vec<Failure>| {
        // SAFETY: i < len, so base.add(i) is in bounds and aligned. All buffer
        // access is volatile to preserve the read-before-write ordering the
        // moving inversion depends on.
        let cell = unsafe { base.add(i) };
        if let Some(expected) = phase.expect {
            let actual = unsafe { ptr::read_volatile(cell) };
            if actual != expected {
                failures.push(Failure {
                    addr: base_addr + i * 8,
                    expected,
                    actual,
                    word_index: i,
                    phys_addr: None,
                });
            }
        }
        if let Some(w) = phase.write {
            unsafe { ptr::write_volatile(cell, w) };
        }
    };

    // Failures already present belong to prior phases and are already claimed;
    // only growth since `claimed` is charged against the budget.
    let mut claimed = failures.len();
    let cap_growth = |failures: &mut Vec<Failure>, claimed: &mut usize| -> bool {
        let grown = failures.len() - *claimed;
        if grown == 0 {
            return false;
        }
        let granted = budget.claim(grown);
        *claimed += granted;
        if granted < grown {
            failures.truncate(*claimed);
            budget.mark_overflow();
            return true;
        }
        false
    };

    let step = |i: usize, failures: &mut Vec<Failure>, claimed: &mut usize| -> bool {
        apply(i, failures);
        if i.is_multiple_of(REPORT_CHUNK) {
            if cap_growth(failures, claimed) {
                return true;
            }
            on_activity(i as f64 / len as f64);
            if shutdown::quit_requested() {
                return true;
            }
        }
        false
    };

    match phase.dir {
        Up => {
            for i in 0..len {
                if step(i, failures, &mut claimed) {
                    return true;
                }
            }
        }
        Down => {
            for i in (0..len).rev() {
                if step(i, failures, &mut claimed) {
                    return true;
                }
            }
        }
    }
    // Charge the tail past the last checkpoint.
    cap_growth(failures, &mut claimed)
}

/// Run the full moving-inversions sequence over `buf`. `on_subpass` fires once
/// per completed phase ([`SUB_PASSES`] total).
pub(super) fn run(
    buf: &mut [u64],
    _parallel: bool,
    budget: &FailureBudget,
    on_subpass: &mut impl FnMut(),
    on_activity: &(dyn Fn(f64) + Sync),
) -> Vec<Failure> {
    let base_addr = buf.as_ptr() as usize;
    let mut failures = Vec::new();

    for &seed in SEEDS {
        let inv = !seed;
        let phases = [
            // fill with the seed
            Phase {
                dir: Up,
                expect: None,
                write: Some(seed),
            },
            // forward: expect the seed, write its inverse
            Phase {
                dir: Up,
                expect: Some(seed),
                write: Some(inv),
            },
            // backward: expect the inverse, write the seed back
            Phase {
                dir: Down,
                expect: Some(inv),
                write: Some(seed),
            },
            // final: the seed must have survived the round trip
            Phase {
                dir: Up,
                expect: Some(seed),
                write: None,
            },
        ];

        for phase in phases {
            let stop = sweep(buf, phase, base_addr, &mut failures, budget, on_activity);
            on_subpass();
            if stop || budget.is_exhausted() || shutdown::quit_requested() {
                return failures;
            }
        }
    }
    failures
}

#[cfg(test)]
mod tests {
    use super::*;

    static NOOP_ACTIVITY: fn(f64) = |_| {};

    mod sweep {
        use assert2::{assert, check};

        use super::*;

        #[test]
        fn fill_writes_seed_to_every_cell() {
            let mut buf = vec![0u64; 16];
            let base = buf.as_ptr() as usize;
            let mut failures = Vec::new();
            let stop = sweep(
                &mut buf,
                Phase {
                    dir: Up,
                    expect: None,
                    write: Some(0xAAAA_AAAA_AAAA_AAAA),
                },
                base,
                &mut failures,
                &FailureBudget::unlimited(),
                &NOOP_ACTIVITY,
            );
            check!(!stop);
            assert!(failures.is_empty());
            assert!(buf.iter().all(|&w| w == 0xAAAA_AAAA_AAAA_AAAA));
        }

        #[test]
        fn verify_only_reports_mismatch_ascending_and_leaves_buffer() {
            let mut buf = vec![0u64; 8]; // holds 0, we expect all-ones
            let base = buf.as_ptr() as usize;
            let mut failures = Vec::new();
            sweep(
                &mut buf,
                Phase {
                    dir: Up,
                    expect: Some(u64::MAX),
                    write: None,
                },
                base,
                &mut failures,
                &FailureBudget::unlimited(),
                &NOOP_ACTIVITY,
            );
            assert!(failures.len() == 8);
            let indices: Vec<usize> = failures.iter().map(|f| f.word_index).collect();
            check!(indices == vec![0, 1, 2, 3, 4, 5, 6, 7]);
            check!(failures[0].expected == u64::MAX);
            check!(failures[0].actual == 0);
            check!(failures[2].addr == base + 2 * 8);
            // A pure verify must not mutate the buffer.
            assert!(buf.iter().all(|&w| w == 0));
        }

        #[test]
        fn verify_reports_descending_order() {
            let mut buf = vec![0u64; 5];
            let base = buf.as_ptr() as usize;
            let mut failures = Vec::new();
            sweep(
                &mut buf,
                Phase {
                    dir: Down,
                    expect: Some(u64::MAX),
                    write: None,
                },
                base,
                &mut failures,
                &FailureBudget::unlimited(),
                &NOOP_ACTIVITY,
            );
            let indices: Vec<usize> = failures.iter().map(|f| f.word_index).collect();
            check!(indices == vec![4, 3, 2, 1, 0]);
        }

        #[test]
        fn read_expected_then_write_inverse() {
            let mut buf = vec![0xAAAA_AAAA_AAAA_AAAAu64; 16];
            let base = buf.as_ptr() as usize;
            let mut failures = Vec::new();
            let stop = sweep(
                &mut buf,
                Phase {
                    dir: Up,
                    expect: Some(0xAAAA_AAAA_AAAA_AAAA),
                    write: Some(0x5555_5555_5555_5555),
                },
                base,
                &mut failures,
                &FailureBudget::unlimited(),
                &NOOP_ACTIVITY,
            );
            check!(!stop);
            assert!(failures.is_empty());
            assert!(buf.iter().all(|&w| w == 0x5555_5555_5555_5555));
        }

        #[test]
        fn corruption_before_the_read_is_caught() {
            let mut buf = vec![0xFFu64; 32];
            let base = buf.as_ptr() as usize;
            let mut failures = Vec::new();
            buf[17] = 0xFF ^ (1 << 3); // one flipped bit vs the expected 0xFF
            sweep(
                &mut buf,
                Phase {
                    dir: Up,
                    expect: Some(0xFF),
                    write: Some(0),
                },
                base,
                &mut failures,
                &FailureBudget::unlimited(),
                &NOOP_ACTIVITY,
            );
            assert!(failures.len() == 1);
            check!(failures[0].word_index == 17);
            check!(failures[0].flipped_bits() == 1);
        }

        #[test]
        fn empty_buffer_is_a_noop() {
            let mut buf: Vec<u64> = vec![];
            let mut failures = Vec::new();
            let stop = sweep(
                &mut buf,
                Phase {
                    dir: Up,
                    expect: Some(0),
                    write: Some(0),
                },
                0,
                &mut failures,
                &FailureBudget::unlimited(),
                &NOOP_ACTIVITY,
            );
            check!(!stop);
            assert!(failures.is_empty());
        }

        #[test]
        fn budget_caps_a_wholly_bad_sweep() {
            let mut buf = vec![0u64; 4096]; // all wrong against expected all-ones
            let base = buf.as_ptr() as usize;
            let mut failures = Vec::new();
            let budget = FailureBudget::new(100);
            let stop = sweep(
                &mut buf,
                Phase {
                    dir: Up,
                    expect: Some(u64::MAX),
                    write: None,
                },
                base,
                &mut failures,
                &budget,
                &NOOP_ACTIVITY,
            );
            check!(stop);
            check!(failures.len() == 100);
            check!(budget.overflowed());
            check!(budget.is_exhausted());
        }
    }

    mod full_sequence {
        use assert2::{assert, check};
        use serial_test::serial;

        use super::*;
        use crate::shutdown::{self, QuitReason};

        #[test]
        fn clean_memory_has_no_failures() {
            let mut buf = vec![0u64; 4096];
            let failures = run(
                &mut buf,
                false,
                &FailureBudget::unlimited(),
                &mut || {},
                &NOOP_ACTIVITY,
            );
            assert!(failures.is_empty());
        }

        #[test]
        fn fires_one_subpass_per_phase() {
            let mut buf = vec![0u64; 256];
            let mut count = 0u64;
            run(
                &mut buf,
                false,
                &FailureBudget::unlimited(),
                &mut || count += 1,
                &NOOP_ACTIVITY,
            );
            check!(count == SUB_PASSES);
        }

        #[test]
        #[serial]
        fn quit_stops_the_sequence_early() {
            shutdown::reset();
            let mut buf = vec![0u64; 256];
            let mut count = 0u64;
            run(
                &mut buf,
                false,
                &FailureBudget::unlimited(),
                &mut || {
                    count += 1;
                    if count == 2 {
                        shutdown::request_quit(QuitReason::UserQuit);
                    }
                },
                &NOOP_ACTIVITY,
            );
            shutdown::reset();
            check!(count == 2);
        }
    }
}
