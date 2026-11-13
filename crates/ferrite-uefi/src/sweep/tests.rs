use alloc::vec;
use alloc::vec::Vec;
use core::num::NonZeroUsize;
use core::time::Duration;

use assert2::{assert, check};
use ferrite_mem::Kernel;

use super::*;

/// Small counts and windows, so a pass over a small buffer is quick.
const CFG: Config = Config {
    fixed_iterations: 1,
    byte_iterations: 1,
    random_rounds: 4,
    modulo_rounds: 1,
    move_iterations: 2,
    bus_iterations: 2,
    address_window_bytes: 4 << 10,
    move_window_bytes: 4 << 10,
    ..Config::DEFAULT
};

fn sweep_of_words(words: usize) -> Sweep {
    Sweep::with_buffer(Some(Buffer::host(words)), Kernels::uniform(Kernel::SCALAR), CFG, Preset::Standard, None)
}

#[test]
fn tests_and_percent_advance_through_passes() {
    let mut sweep = sweep_of_words(2048);
    let mut seen: Vec<SweepStatus> = vec![sweep.status()];
    while sweep.status().passes < 2 {
        sweep.step(16 << 10, Instant::default(), None);
        seen.push(sweep.status());
    }
    for pair in seen.windows(2) {
        let [before, after] = pair else { unreachable!() };
        if before.passes == after.passes {
            check!(after.percent >= before.percent);
            check!(after.position.0 >= before.position.0);
        } else {
            check!(after.position.0 == 1);
        }
    }
    let n = seen[0].position.1;
    let mut tests: Vec<_> = seen.iter().filter(|s| s.passes == 0).map(|s| s.position.0).collect();
    tests.dedup();
    check!(tests == (1..=n).collect::<Vec<_>>());
    check!(seen.iter().all(|s| s.faults == 0 && s.first_fault.is_none()));
}

#[test]
fn a_corrupted_word_is_counted_and_reported_with_its_test() {
    let mut sweep = sweep_of_words(2048);
    let base = sweep.run.as_ref().map(|r| r.suite.region().base()).unwrap();
    // Solid bits fills and then verifies, so a word changed between steps reads back wrong.
    while sweep.status().test != "solid bits" {
        sweep.step(64, Instant::default(), None);
    }
    sweep.step(64 << 10, Instant::default(), None);
    let word = 100;
    let ptr = sweep.run.as_ref().map(|r| r.suite.region().base().get() as *mut u64).unwrap();
    // SAFETY: inside the host buffer; no step runs while it is written.
    let expected = unsafe { ptr.add(word).read() };
    // SAFETY: as above.
    unsafe { ptr.add(word).write(expected ^ 1 << 7) };
    while sweep.status().test == "solid bits" {
        sweep.step(64 << 10, Instant::default(), None);
    }
    let status = sweep.status();
    check!(status.faults == 1);
    assert!(let Some(first) = status.first_fault);
    check!(first.fault.addr.get() == base.get() + word * WORD);
    check!(first.fault.expected == expected);
    check!(first.fault.actual == expected ^ 1 << 7);
    check!(first.test == "solid bits");
    check!(status.analysis == Classification::StuckBits { mask: 1 << 7 });
}

#[test]
fn a_waiting_suite_does_no_work_until_its_time() {
    let fade = Duration::from_secs(5);
    let mut sweep = Sweep::with_buffer(
        Some(Buffer::host(1024)),
        Kernels::uniform(Kernel::SCALAR),
        Config { fade, ..CFG },
        Preset::Thorough,
        None,
    );
    let t0 = Instant::default();
    while sweep.waiting_until().is_none() {
        sweep.step(64 << 10, t0, None);
    }
    check!(sweep.status().test == "bit fade");
    check!(sweep.waiting_until() == Some(t0 + fade));
    check!(!sweep.has_work(t0));
    let before = sweep.status();
    sweep.step(64 << 10, t0, None);
    check!(sweep.status() == before);
    check!(sweep.has_work(t0 + fade));
}

#[test]
fn no_buffer_means_no_work() {
    let mut sweep = Sweep::with_buffer(None, Kernels::uniform(Kernel::SCALAR), CFG, Preset::Standard, None);
    sweep.step(4096, Instant::default(), None);
    let status = sweep.status();
    check!(!sweep.has_work(Instant::default()));
    check!(status.bytes == 0);
    check!(status.passes == 0);
}

#[test]
fn allocation_halves_until_it_fits() {
    let mut tried = Vec::new();
    let buf = alloc_largest(64 << 10, 4 << 10, |bytes| {
        tried.push(bytes);
        (bytes <= 10 << 10).then_some(bytes)
    });
    check!(tried == [64 << 10, 32 << 10, 16 << 10, 8 << 10]);
    check!(buf == Some(8 << 10));
}

#[test]
fn allocation_gives_up_below_the_minimum() {
    let mut tried = Vec::new();
    let buf = alloc_largest(24 << 10, 8 << 10, |bytes| {
        tried.push(bytes);
        None::<usize>
    });
    check!(tried == [24 << 10, 12 << 10, 8 << 10]);
    check!(buf.is_none());
}

/// A clock that stays at one instant.
#[derive(Clone, Copy)]
struct Fixed;

impl ferrite_hal::Clock for Fixed {
    fn now(&self) -> Instant {
        Instant::default()
    }
}

#[test]
fn faults_merged_from_several_cores_keep_their_finder_and_classify_together() {
    use ferrite_mem::suite::{Mode, TestKind, shares};

    use crate::crew::{Crew, Job, Seat, Turn};

    let words = 3 * 512;
    let buffer = Buffer::host(words);
    // SAFETY: the buffer is ours, and is only written below while every job waits at a gate.
    let mut region = unsafe { Region::from_raw(buffer.base.as_ptr(), words) }.unwrap();
    let kind = TestKind::SolidBits;
    assert!(kind.mode() == Mode::Split);
    let parts = NonZeroUsize::new(3).unwrap();
    let split: Vec<(Span, Region<'_>)> = shares(&mut region, kind, &CFG, parts).collect();
    let starts: Vec<usize> = split.iter().map(|(s, _)| s.start).collect();
    let crew = Crew::default();
    let seats: Vec<Seat> = split.iter().map(|_| Seat::default()).collect();
    let jobs: Vec<Job<'_, Fixed>> = split
        .into_iter()
        .zip(&seats)
        .map(|((span, region), seat)| {
            let test = Test::for_share(kind, CFG, 0, words, span);
            Job::new(test, region, Kernels::uniform(Kernel::SCALAR), 16 << 10, Fixed, &crew, seat)
        })
        .collect();
    let mut workers: Vec<_> = jobs.iter().map(|j| j.claim().unwrap()).collect();
    let mut gated = 0;
    // Every core fills 0, then waits at the gate before verifying it.
    while gated < workers.len() {
        gated = workers.iter_mut().map(|w| w.turn(None)).filter(|&t| t == Turn::Gate).count();
    }
    let flip = |word: usize, mask: u64| {
        // SAFETY: inside the buffer, and every job is held at the gate.
        unsafe { buffer.base.as_ptr().add(word).write(mask) };
    };
    // Bit 7 stuck high on two words of the second share and one of the third.
    let bad = [starts[1] + 3, starts[1] + 40, starts[2] + 9];
    for &w in &bad {
        flip(w, 1 << 7);
    }
    let mut ended = false;
    while !ended {
        ended = crew.coordinate(&seats);
        for w in &mut workers {
            w.turn(None);
        }
    }
    let cpus = [0, 5, 7];
    let mut tally = Tally::default();
    let mut absorbed = vec![0; seats.len()];
    absorb(&mut tally, &seats, &mut absorbed, &cpus, kind.name(), 0);
    let base = SweepStatus { cpus: 3, mode: Mode::Split, ..SweepStatus::IDLE };
    let total = seats.iter().map(Seat::done).sum();
    let status = live(&base, &seats, &tally, 2, (0, total, total));
    check!(seats.iter().map(Seat::faults).collect::<Vec<_>>() == [0, 2, 1]);
    check!(status.faults == 2 + 3, "faults found before the test are kept");
    check!(status.test_percent == 100);
    assert!(let Some(first) = status.first_fault);
    check!(first.fault.addr.get() == buffer.base.as_ptr().wrapping_add(bad[0]).addr());
    check!(first.cpu == 5);
    check!(first.test == "solid bits");
    check!(status.analysis == Classification::StuckBits { mask: 1 << 7 });
    absorb(&mut tally, &seats, &mut absorbed, &cpus, kind.name(), 0);
    check!(tally.stats.faults() == 3, "a fault is absorbed once");
}

#[test]
fn history_records_every_test_and_each_finished_pass() {
    let mut sweep = sweep_of_words(2048);
    let tests_per_pass = sweep.status().position.1;
    let mut ms = 0;
    while sweep.status().passes < 2 {
        ms += 1;
        sweep.step(16 << 10, Instant::from_epoch(Duration::from_millis(ms)), None);
    }
    // The last test of pass 1 is recorded once the next step notices the change.
    sweep.step(64, Instant::from_epoch(Duration::from_millis(ms + 1)), None);
    let history = &sweep.history;
    check!(history.passes.iter().map(|p| p.pass).collect::<Vec<_>>() == vec![0, 1]);
    check!(history.tests.len() == 2 * tests_per_pass);
    check!(history.latest_tests().count() == tests_per_pass);
    check!(history.latest_tests().all(|r| r.pass == 1));
}

#[test]
fn restart_clears_results_and_switches_the_preset() {
    let mut sweep = sweep_of_words(2048);
    while sweep.status().passes < 1 {
        sweep.step(16 << 10, Instant::default(), None);
    }
    sweep.restart(Preset::Quick);
    let status = sweep.status();
    check!(status.passes == 0);
    check!(status.position.0 == 1);
    check!(status.preset == Preset::Quick);
    check!(sweep.history.tests.is_empty());
}

#[test]
fn a_paused_sweep_does_no_work_until_resumed() {
    let mut sweep = sweep_of_words(2048);
    sweep.pause();
    let before = sweep.status();
    sweep.step(64 << 10, Instant::default(), None);
    check!(!sweep.has_work(Instant::default()));
    check!(sweep.status() == before);
    sweep.resume();
    sweep.step(64 << 10, Instant::default(), None);
    check!(sweep.status() != before);
}
