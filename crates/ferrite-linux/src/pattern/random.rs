//! Random fill: write a reproducible pseudorandom sequence to every word, then
//! replay the same sequence to verify it. Random data exercises bit
//! combinations that fixed patterns never produce, exposing data-dependent
//! faults, and gives two distinct addresses different contents (so an address
//! decoder fault that aliases them shows up as a mismatch).
//!
//! The generator is xoshiro256**. Reproducibility is the whole point: the seed
//! is reported so a failing run can be replayed exactly. Parallel chunks each
//! get an independent stream 2^128 outputs apart via `jump()`, and successive
//! passes are 2^192 apart via `long_jump()`, so both fill and verify regenerate
//! identical values regardless of thread count.

use std::ptr;

use rand_xoshiro::Xoshiro256StarStar;
use rand_xoshiro::rand_core::{Rng, SeedableRng};
use rayon::prelude::*;

use crate::ops::CHUNK_WORDS as CHUNK;
use crate::shutdown;
use crate::{Failure, FailureBudget};

/// Number of `CHUNK`-sized regions the buffer splits into.
const fn chunk_count(len: usize) -> usize {
    len.div_ceil(CHUNK)
}

/// One independent generator per chunk, each `jump()`ed 2^128 outputs past the
/// previous so parallel chunks never overlap. Rebuilding from the same `root`
/// reproduces the identical set, which is what lets verify replay fill.
fn chunk_rngs(root: &Xoshiro256StarStar, chunks: usize) -> Vec<Xoshiro256StarStar> {
    let mut next = root.clone();
    (0..chunks)
        .map(|_| {
            let cur = next.clone();
            next.jump();
            cur
        })
        .collect()
}

/// Write the pseudorandom sequence rooted at `root` to every word.
fn fill_pass(
    buf: &mut [u64],
    root: &Xoshiro256StarStar,
    parallel: bool,
    on_activity: &(dyn Fn(f64) + Sync),
) {
    let total = buf.len();
    let rngs = chunk_rngs(root, chunk_count(total));

    let fill_chunk = |ci: usize, chunk: &mut [u64], mut rng: Xoshiro256StarStar| {
        for word in chunk.iter_mut() {
            // SAFETY: `word` is a valid, aligned element of `buf`; volatile
            // stores preserve every write against compiler elision.
            unsafe { ptr::write_volatile(std::ptr::from_mut::<u64>(word), rng.next_u64()) };
        }
        on_activity((ci * CHUNK) as f64 / total as f64);
    };

    if parallel {
        buf.par_chunks_mut(CHUNK)
            .zip(rngs.into_par_iter())
            .enumerate()
            .for_each(|(ci, (chunk, rng))| fill_chunk(ci, chunk, rng));
    } else {
        for (ci, (chunk, rng)) in buf.chunks_mut(CHUNK).zip(rngs).enumerate() {
            fill_chunk(ci, chunk, rng);
        }
    }
}

/// Replay the sequence rooted at `root` and report every word that read back a
/// different value. Returns `true`-signalled early stop via the budget, matching
/// the scalar fill/verify orchestration.
fn verify_pass(
    buf: &[u64],
    root: &Xoshiro256StarStar,
    parallel: bool,
    base_addr: usize,
    budget: &FailureBudget,
    on_activity: &(dyn Fn(f64) + Sync),
) -> Vec<Failure> {
    let total = buf.len();
    let rngs = chunk_rngs(root, chunk_count(total));

    let verify_chunk = |ci: usize, chunk: &[u64], mut rng: Xoshiro256StarStar| -> Vec<Failure> {
        let chunk_start = ci * CHUNK;
        on_activity(chunk_start as f64 / total as f64);
        if budget.is_exhausted() {
            return Vec::new();
        }
        let mut f = Vec::new();
        for (i, word) in chunk.iter().enumerate() {
            let expected = rng.next_u64();
            // SAFETY: `word` is a valid, aligned element of `buf`; volatile
            // loads force the read against compiler elision.
            let actual = unsafe { ptr::read_volatile(std::ptr::from_ref::<u64>(word)) };
            if actual != expected {
                f.push(Failure {
                    addr: base_addr + (chunk_start + i) * 8,
                    expected,
                    actual,
                    word_index: chunk_start + i,
                    phys_addr: None,
                });
            }
        }
        budget.cap(&mut f);
        f
    };

    if parallel {
        buf.par_chunks(CHUNK)
            .zip(rngs.into_par_iter())
            .enumerate()
            .flat_map_iter(|(ci, (chunk, rng))| verify_chunk(ci, chunk, rng))
            .collect()
    } else {
        let mut failures = Vec::new();
        for (ci, (chunk, rng)) in buf.chunks(CHUNK).zip(rngs).enumerate() {
            if budget.is_exhausted() {
                break;
            }
            failures.append(&mut verify_chunk(ci, chunk, rng));
        }
        failures
    }
}

/// Run `passes` seeded fill-and-verify rounds over `buf`. Each round uses a
/// stream `long_jump()`ed 2^192 outputs past the last, all derived from `seed`.
/// `on_subpass` fires twice per round (fill, verify).
pub(super) fn run(
    buf: &mut [u64],
    parallel: bool,
    seed: u64,
    passes: usize,
    budget: &FailureBudget,
    on_subpass: &mut impl FnMut(),
    on_activity: &(dyn Fn(f64) + Sync),
) -> Vec<Failure> {
    let base_addr = buf.as_ptr() as usize;
    let mut failures = Vec::new();
    let mut master = Xoshiro256StarStar::seed_from_u64(seed);

    for _ in 0..passes {
        let root = master.clone();
        master.long_jump();

        fill_pass(buf, &root, parallel, on_activity);
        on_subpass();
        if budget.is_exhausted() || shutdown::quit_requested() {
            return failures;
        }

        failures.append(&mut verify_pass(
            buf,
            &root,
            parallel,
            base_addr,
            budget,
            on_activity,
        ));
        on_subpass();
        if budget.is_exhausted() || shutdown::quit_requested() {
            return failures;
        }
    }
    failures
}

#[cfg(test)]
mod tests {
    use super::*;

    static NOOP_ACTIVITY: fn(f64) = |_| {};

    fn root(seed: u64) -> Xoshiro256StarStar {
        Xoshiro256StarStar::seed_from_u64(seed)
    }

    mod passes {
        use assert2::{assert, check};

        use super::*;

        #[test]
        fn fill_is_deterministic_across_serial_and_parallel() {
            // The same root must produce the same buffer regardless of thread
            // count, or a parallel fill could never be verified by a serial
            // replay (or vice versa).
            let mut serial = vec![0u64; 5000];
            let mut parallel = vec![0u64; 5000];
            fill_pass(&mut serial, &root(0x1234), false, &NOOP_ACTIVITY);
            fill_pass(&mut parallel, &root(0x1234), true, &NOOP_ACTIVITY);
            assert!(serial == parallel);
        }

        #[test]
        fn fill_is_not_all_one_value() {
            let mut buf = vec![0u64; 4096];
            fill_pass(&mut buf, &root(1), false, &NOOP_ACTIVITY);
            let first = buf[0];
            check!(buf.iter().any(|&w| w != first), "fill produced a constant");
        }

        #[test]
        fn different_seeds_differ() {
            let mut a = vec![0u64; 4096];
            let mut b = vec![0u64; 4096];
            fill_pass(&mut a, &root(1), false, &NOOP_ACTIVITY);
            fill_pass(&mut b, &root(2), false, &NOOP_ACTIVITY);
            check!(a != b);
        }

        #[test]
        fn fill_then_verify_is_clean_serial() {
            let mut buf = vec![0u64; 5000];
            fill_pass(&mut buf, &root(7), false, &NOOP_ACTIVITY);
            let base = buf.as_ptr() as usize;
            let failures = verify_pass(
                &buf,
                &root(7),
                false,
                base,
                &FailureBudget::unlimited(),
                &NOOP_ACTIVITY,
            );
            assert!(failures.is_empty());
        }

        #[test]
        fn fill_then_verify_is_clean_parallel() {
            let mut buf = vec![0u64; 5000];
            fill_pass(&mut buf, &root(7), true, &NOOP_ACTIVITY);
            let base = buf.as_ptr() as usize;
            let failures = verify_pass(
                &buf,
                &root(7),
                true,
                base,
                &FailureBudget::unlimited(),
                &NOOP_ACTIVITY,
            );
            assert!(failures.is_empty());
        }

        #[test]
        fn corruption_is_caught_with_correct_index() {
            let mut buf = vec![0u64; 4096];
            fill_pass(&mut buf, &root(9), false, &NOOP_ACTIVITY);
            let base = buf.as_ptr() as usize;
            let good = buf[1234];
            buf[1234] = !good; // guaranteed different from the expected word
            let failures = verify_pass(
                &buf,
                &root(9),
                false,
                base,
                &FailureBudget::unlimited(),
                &NOOP_ACTIVITY,
            );
            assert!(failures.len() == 1);
            check!(failures[0].word_index == 1234);
            check!(failures[0].actual == !good);
            check!(failures[0].addr == base + 1234 * 8);
        }

        #[test]
        fn budget_caps_wholly_bad_verify() {
            // Verify against a different root than was filled: every word
            // mismatches, so the budget must bound the collected failures.
            let mut buf = vec![0u64; 4096];
            fill_pass(&mut buf, &root(1), false, &NOOP_ACTIVITY);
            let base = buf.as_ptr() as usize;
            let budget = FailureBudget::new(100);
            let failures = verify_pass(&buf, &root(2), false, base, &budget, &NOOP_ACTIVITY);
            check!(failures.len() <= 100);
            check!(budget.is_exhausted());
        }

        #[test]
        fn empty_buffer_is_clean() {
            let mut buf: Vec<u64> = vec![];
            fill_pass(&mut buf, &root(1), false, &NOOP_ACTIVITY);
            let failures = verify_pass(
                &buf,
                &root(1),
                false,
                0,
                &FailureBudget::unlimited(),
                &NOOP_ACTIVITY,
            );
            assert!(failures.is_empty());
        }
    }

    mod full_run {
        use assert2::{assert, check};
        use serial_test::serial;

        use super::*;
        use crate::shutdown::{self, QuitReason};

        #[test]
        fn clean_memory_has_no_failures() {
            let mut buf = vec![0u64; 5000];
            let failures = run(
                &mut buf,
                false,
                0xDEAD,
                3,
                &FailureBudget::unlimited(),
                &mut || {},
                &NOOP_ACTIVITY,
            );
            assert!(failures.is_empty());
        }

        #[test]
        fn fires_two_subpasses_per_pass() {
            let mut buf = vec![0u64; 256];
            let mut count = 0u64;
            run(
                &mut buf,
                false,
                1,
                4,
                &FailureBudget::unlimited(),
                &mut || count += 1,
                &NOOP_ACTIVITY,
            );
            check!(count == 8); // 4 passes x (fill + verify)
        }

        #[test]
        fn seed_is_reproducible() {
            // Two runs with the same seed corrupt-detect identically: fill, then
            // an independent replay via a fresh run over a copy must agree.
            let mut a = vec![0u64; 4096];
            let mut b = vec![0u64; 4096];
            run(
                &mut a,
                false,
                0x5EED,
                1,
                &FailureBudget::unlimited(),
                &mut || {},
                &NOOP_ACTIVITY,
            );
            run(
                &mut b,
                false,
                0x5EED,
                1,
                &FailureBudget::unlimited(),
                &mut || {},
                &NOOP_ACTIVITY,
            );
            // After a full run the buffer holds the last pass's sequence; same
            // seed must leave identical contents.
            check!(a == b);
        }

        #[test]
        #[serial]
        fn quit_stops_early() {
            shutdown::reset();
            let mut buf = vec![0u64; 256];
            let mut count = 0u64;
            run(
                &mut buf,
                false,
                1,
                8,
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
