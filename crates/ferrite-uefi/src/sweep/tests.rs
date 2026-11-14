use alloc::vec;
use alloc::vec::Vec;
use core::num::NonZeroUsize;
use core::time::Duration;

use assert2::{assert, check};
use ferrite_efi::memory::Block;
use ferrite_mem::{Kernel, Regions};
use rstest::{fixture, rstest};

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

/// One host block per entry of `words`.
fn host_blocks(words: &[usize]) -> Claim {
    Claim::new(words.iter().map(|&w| Block::heap(w)).collect())
}

fn sweep_of_words(words: usize) -> Sweep {
    Sweep::with_buffer(Some(host_blocks(&[words])), Kernels::uniform(Kernel::SCALAR), CFG, Preset::Standard, None)
}

#[fixture]
fn sweep() -> Sweep {
    sweep_of_words(2048)
}

#[rstest]
fn tests_and_percent_advance_through_passes(mut sweep: Sweep) {
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

fn sweep_of_blocks(words: &[usize]) -> Sweep {
    Sweep::with_buffer(Some(host_blocks(words)), Kernels::uniform(Kernel::SCALAR), CFG, Preset::Standard, None)
}

#[test]
#[expect(clippy::used_underscore_binding, reason = "the buffer field only owns memory outside tests")]
fn a_corrupted_word_is_counted_and_reported_with_its_test() {
    let mut sweep = sweep_of_blocks(&[1024, 512, 1024]);
    check!(sweep.status().bytes == 2560 * WORD, "the status counts every block");
    let block = sweep.run.as_ref().map(|r| r._buffer.blocks()[2].base().as_ptr()).unwrap();
    // Solid bits fills and then verifies, so a word changed between steps reads back wrong.
    while sweep.status().test != "solid bits" {
        sweep.step(64, Instant::default(), None);
    }
    sweep.step(64 << 10, Instant::default(), None);
    let word = 100;
    // SAFETY: inside the third host block; no step runs while it is written.
    let expected = unsafe { block.add(word).read() };
    // SAFETY: as above.
    unsafe { block.add(word).write(expected ^ 1 << 7) };
    while sweep.status().test == "solid bits" {
        sweep.step(64 << 10, Instant::default(), None);
    }
    let status = sweep.status();
    check!(status.faults == 1);
    assert!(let Some(first) = status.first_fault);
    check!(first.fault.addr.get() == block.addr() + word * WORD, "found in the third block, at its own address");
    check!(first.fault.expected == expected);
    check!(first.fault.actual == expected ^ 1 << 7);
    check!(first.test == "solid bits");
    check!(status.analysis == Classification::StuckBits { mask: 1 << 7 });
}

#[test]
fn a_waiting_suite_does_no_work_until_its_time() {
    let fade = Duration::from_secs(5);
    let mut sweep = Sweep::with_buffer(
        Some(host_blocks(&[1024])),
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

#[rstest]
#[case::a_floor_and_a_share(16 << 30, RESERVE_FLOOR + (512 << 20))]
#[case::at_most_half(100 << 20, 50 << 20)]
#[case::nothing_to_reserve(0, 0)]
fn the_reserve_is_a_floor_and_a_share_but_at_most_half(#[case] total: u64, #[case] reserve: u64) {
    check!(reserve_for(total) == reserve);
}

#[rstest]
#[case::halves_until_it_fits(64 << 10, 4 << 10, 10 << 10, &[64 << 10, 32 << 10, 16 << 10, 8 << 10], Some(8 << 10))]
#[case::gives_up_below_the_minimum(24 << 10, 8 << 10, 0, &[24 << 10, 12 << 10, 8 << 10], None)]
fn allocation_halves_until_it_fits_or_gives_up(
    #[case] start: usize,
    #[case] minimum: usize,
    #[case] fits: usize,
    #[case] expected_tries: &[usize],
    #[case] expected: Option<usize>,
) {
    let mut tried = Vec::new();
    let buf = alloc_largest(start, minimum, |bytes| {
        tried.push(bytes);
        (bytes <= fits).then_some(bytes)
    });
    check!(tried == expected_tries);
    check!(buf == expected);
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

    use ferrite_mem::crew::{Crew, Job, Seat, Turn};

    let words = 3 * 512;
    let buffer = host_blocks(&[words]);
    let ptr = buffer.blocks()[0].base().as_ptr();
    // SAFETY: the buffer is ours, and is only written below while every job waits at a gate.
    let region = unsafe { Region::from_raw(ptr, words) }.unwrap();
    let mut regions = Regions::from(region);
    let layout = regions.layout();
    let kind = TestKind::SolidBits;
    assert!(kind.mode() == Mode::Split);
    let parts = NonZeroUsize::new(3).unwrap();
    let split: Vec<(Span, Regions<'_>)> = shares(&mut regions, kind, &CFG, parts).collect();
    let starts: Vec<usize> = split.iter().map(|(s, _)| s.start).collect();
    let crew = Crew::default();
    let seats: Vec<Seat> = split.iter().map(|_| Seat::default()).collect();
    let jobs: Vec<Job<'_, Fixed>> = split
        .into_iter()
        .zip(&seats)
        .map(|((span, regions), seat)| {
            let test = Test::for_share(kind, CFG, 0, &layout, span);
            Job::new(vec![test], regions, Kernels::uniform(Kernel::SCALAR), 16 << 10, Fixed, &crew, seat)
        })
        .collect();
    let mut workers: Vec<_> = jobs.iter().map(|j| j.claim().unwrap()).collect();
    crew.begin_round();
    for w in &mut workers {
        w.begin(crew.round());
    }
    let mut gated = 0;
    // Every core fills 0, then waits at the gate before verifying it.
    while gated < workers.len() {
        gated = workers.iter_mut().map(|w| w.turn(None)).filter(|&t| t == Turn::Gate).count();
    }
    let flip = |word: usize, mask: u64| {
        // SAFETY: inside the buffer, and every job is held at the gate.
        unsafe { ptr.add(word).write(mask) };
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
    check!(first.fault.addr.get() == ptr.wrapping_add(bad[0]).addr());
    check!(first.cpu == 5);
    check!(first.test == "solid bits");
    check!(status.analysis == Classification::StuckBits { mask: 1 << 7 });
    absorb(&mut tally, &seats, &mut absorbed, &cpus, kind.name(), 0);
    check!(tally.stats.faults() == 3, "a fault is absorbed once");
}

#[rstest]
fn history_records_every_test_and_each_finished_pass(mut sweep: Sweep) {
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

#[rstest]
fn restart_clears_results_and_switches_the_preset(mut sweep: Sweep) {
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

#[rstest]
fn a_paused_sweep_does_no_work_until_resumed(mut sweep: Sweep) {
    sweep.pause();
    let before = sweep.status();
    sweep.step(64 << 10, Instant::default(), None);
    check!(!sweep.has_work(Instant::default()));
    check!(sweep.status() == before);
    sweep.resume();
    sweep.step(64 << 10, Instant::default(), None);
    check!(sweep.status() != before);
}

#[rstest]
fn a_goal_of_time_takes_no_more_work_once_the_time_is_up(mut sweep: Sweep) {
    sweep.set_goal(Goal { time: Some(Duration::from_secs(10)), ..Goal::UNLIMITED });
    let start = Instant::from_epoch(Duration::from_secs(1));
    sweep.step(64, start, None);
    check!(sweep.has_work(start + Duration::from_secs(9)));
    let up = start + Duration::from_secs(10);
    check!(!sweep.has_work(up));
    let before = sweep.status();
    sweep.step(64 << 10, up, None);
    check!(sweep.status() == before, "no test goes on after the goal is met");
}

#[rstest]
#[expect(clippy::used_underscore_binding, reason = "the buffer field only owns memory outside tests")]
fn a_goal_of_the_first_fault_takes_no_more_work_once_there_is_one(mut sweep: Sweep) {
    sweep.set_goal(Goal { stop_on_fault: true, ..Goal::UNLIMITED });
    let now = Instant::from_epoch(Duration::from_secs(1));
    let block = sweep.run.as_ref().map(|r| r._buffer.blocks()[0].base().as_ptr()).unwrap();
    while sweep.status().test != "solid bits" {
        sweep.step(64, now, None);
    }
    sweep.step(64 << 10, now, None);
    check!(sweep.has_work(now), "nothing wrong yet");
    // SAFETY: inside the host block; no step runs while it is written.
    unsafe { block.add(100).write(!0) };
    sweep.step(64 << 10, now, None);
    check!(sweep.status().faults > 0);
    check!(!sweep.has_work(now));
    let before = sweep.status();
    sweep.step(64 << 10, now, None);
    check!(sweep.status() == before);
}

#[rstest]
fn restarting_with_the_memtest86_plus_preset_swaps_the_tests_and_counts(mut sweep: Sweep) {
    sweep.restart(Preset::Memtest86Plus);
    let status = sweep.status();
    check!(status.preset == Preset::Memtest86Plus);
    check!(status.position == (1, 11));
    check!(sweep.run.as_ref().map(|r| r.suite.config()) == Some(Preset::Memtest86Plus.config()));
}

#[test]
fn a_sweep_over_several_blocks_tests_each_of_them_every_pass() {
    let mut sweep = sweep_of_blocks(&[1024, 512, 1024]);
    let mut ms = 0;
    while sweep.status().passes < 1 {
        ms += 1;
        sweep.step(16 << 10, Instant::from_epoch(Duration::from_millis(ms)), None);
    }
    let status = sweep.status();
    check!(status.faults == 0 && status.bytes == 2560 * WORD);
}

mod rotation {
    use alloc::rc::Rc;
    use core::cell::RefCell;

    use assert2::{assert, check};
    use ferrite_efi::memory::ranges::Range;

    use super::*;
    use crate::sweep::rotation::{Closed, Opened, Pages};

    const MAIN_WORDS: usize = 2048;
    const RESERVE_WORDS: usize = 1024;

    /// What the fake firmware was asked, in order.
    #[derive(Clone, Default)]
    struct Calls(Rc<RefCell<Vec<&'static str>>>);

    impl Calls {
        fn list(&self) -> Vec<&'static str> {
            self.0.borrow().clone()
        }
    }

    struct Fake {
        calls: Calls,
        refuse: bool,
        kept: u64,
    }

    impl Pages for Fake {
        fn open(&mut self, _main: &mut Claim, lend: u64) -> Option<Opened> {
            self.calls.0.borrow_mut().push("open");
            (!self.refuse).then(|| Opened {
                reserve: host_blocks(&[RESERVE_WORDS]),
                lent: vec![Range::new(1 << 20, (lend / 4096) as usize)],
                free: lend,
            })
        }

        fn close(&mut self, _main: &mut Claim, _reserve: Claim, _lent: &[Range]) -> Closed {
            self.calls.0.borrow_mut().push("close");
            Closed { kept: self.kept, free: 7 << 20 }
        }
    }

    #[fixture]
    fn open_rig() -> (Sweep, Calls) {
        rotating(false, 0)
    }

    fn rotating(refuse: bool, kept: u64) -> (Sweep, Calls) {
        let calls = Calls::default();
        let pages = Fake { calls: calls.clone(), refuse, kept };
        (sweep_of_words(MAIN_WORDS).with_pages(pages, 1 << 20), calls)
    }

    /// Step in 1 ms ticks until `done`.
    fn step_until(sweep: &mut Sweep, mut done: impl FnMut(&Sweep) -> bool) {
        let mut ms = 0;
        while !done(sweep) {
            ms += 1;
            sweep.step(16 << 10, Instant::from_epoch(Duration::from_millis(ms)), None);
        }
    }

    #[rstest]
    fn each_pass_is_followed_by_a_pass_over_the_reserve(open_rig: (Sweep, Calls)) {
        let (mut sweep, calls) = open_rig;
        let tests = sweep.status().position.1;
        step_until(&mut sweep, |s| s.rotation.is_open());
        check!(calls.list() == ["open"], "nothing rotates before the first pass has ended");
        let status = sweep.status();
        check!(status.bytes == RESERVE_WORDS * WORD, "the tests run over the reserve");
        check!(status.passes == 1, "the pass count stays the sweep's");
        step_until(&mut sweep, |s| !s.rotation.is_open());
        check!(calls.list() == ["open", "close"]);
        let status = sweep.status();
        check!(status.bytes == MAIN_WORDS * WORD);
        check!((status.passes, status.position.0) == (1, 1), "the sweep goes on with pass 1 from its first test");
        check!(sweep.history.tests.len() == tests, "the reserve's tests are not the sweep's");
        step_until(&mut sweep, |s| s.rotation.is_open());
        check!(calls.list() == ["open", "close", "open"]);
        check!(sweep.status().passes == 2);
    }

    /// Step and look at `sweep` in 1 ms ticks until `done`.
    fn watch_until(sweep: &mut Sweep, mut done: impl FnMut(&Sweep) -> bool) {
        let mut ms = 0;
        while !done(sweep) {
            ms += 1;
            let now = Instant::from_epoch(Duration::from_millis(ms));
            sweep.tick(now);
            sweep.step(16 << 10, now, None);
        }
    }

    #[rstest]
    fn a_pass_counts_for_the_goal_only_once_its_reserve_has_been_tested(open_rig: (Sweep, Calls)) {
        let (mut sweep, _) = open_rig;
        sweep.set_goal(Goal { passes: core::num::NonZeroU64::new(1), ..Goal::UNLIMITED });
        // The main pass ends: the next look finds the rotation due but not yet opened.
        watch_until(&mut sweep, |s| s.run.as_ref().is_some_and(|r| r.suite.passes() == 1));
        sweep.tick(Instant::from_epoch(Duration::from_secs(1)));
        let o = sweep.status().outlook;
        check!(o.settled == 0, "one pass done, its rotation still to come");
        check!(o.goal.met(o.settled, Duration::ZERO, 0).is_none());
        watch_until(&mut sweep, |s| s.rotation.is_open());
        sweep.tick(Instant::from_epoch(Duration::from_secs(2)));
        let o = sweep.status().outlook;
        check!(o.settled == 0 && o.reserve, "the reserve is under test");
        watch_until(&mut sweep, |s| !s.rotation.is_open());
        sweep.tick(Instant::from_epoch(Duration::from_secs(3)));
        let o = sweep.status().outlook;
        check!(o.settled == 1 && !o.reserve);
        check!(o.goal.met(o.settled, Duration::ZERO, 0).is_some());
    }

    #[rstest]
    fn a_pass_goal_met_by_the_rotation_ends_the_run_before_the_next_pass_starts(open_rig: (Sweep, Calls)) {
        let (mut sweep, _) = open_rig;
        let tests = sweep.status().position.1;
        sweep.set_goal(Goal { passes: core::num::NonZeroU64::new(1), ..Goal::UNLIMITED });
        let mut now = Instant::from_epoch(Duration::from_secs(1));
        // As the main loop does: look, end the run if the goal is met, else work until the next frame.
        for _ in 0..1_000 {
            sweep.tick(now);
            let o = sweep.status().outlook;
            if o.goal.met(o.settled, Duration::ZERO, 0).is_some() {
                break;
            }
            for _ in 0..1_000 {
                if !sweep.has_work(now) {
                    break;
                }
                sweep.step(16 << 10, now, None);
            }
            now = now + Duration::from_millis(50);
        }
        let status = sweep.status();
        check!(status.outlook.settled == 1, "{:?}", status.outlook.goal);
        check!(!sweep.has_work(now));
        check!((status.position.0, status.test_percent) == (1, 0), "no test of the next pass has begun");
        check!(sweep.history.tests.len() == tests, "the sweep's own history holds the first pass alone");
    }

    #[rstest]
    #[expect(clippy::used_underscore_binding, reason = "the buffer field only owns memory outside tests")]
    fn a_fault_in_the_reserve_is_reported_with_its_address_and_the_pass_it_followed(open_rig: (Sweep, Calls)) {
        let (mut sweep, _) = open_rig;
        step_until(&mut sweep, |s| s.rotation.is_open());
        let block = sweep.run.as_ref().map(|r| r._buffer.blocks()[0].base().as_ptr()).unwrap();
        while sweep.status().test != "solid bits" {
            sweep.step(64, Instant::default(), None);
        }
        sweep.step(64 << 10, Instant::default(), None);
        // SAFETY: inside the reserve's block; no step runs while it is written.
        let expected = unsafe { block.add(40).read() };
        // SAFETY: as above.
        unsafe { block.add(40).write(expected ^ 1 << 3) };
        step_until(&mut sweep, |s| !s.rotation.is_open());
        let status = sweep.status();
        check!(status.faults == 1);
        assert!(let Some(first) = status.first_fault);
        check!(first.fault.addr.get() == block.addr() + 40 * WORD);
        check!(first.pass == 0, "the reserve of pass 0 is tested as pass 0");
    }

    #[test]
    fn a_refused_window_is_not_asked_for_again_in_the_same_pass() {
        let (mut sweep, calls) = rotating(true, 0);
        step_until(&mut sweep, |s| s.status().passes == 2);
        sweep.step(64, Instant::default(), None);
        check!(calls.list() == ["open", "open"], "once after each of the two passes");
        check!(sweep.status().bytes == MAIN_WORDS * WORD);
        check!(!sweep.rotation.is_open());
    }

    #[rstest]
    fn restarting_inside_a_window_ends_it_and_starts_over(open_rig: (Sweep, Calls)) {
        let (mut sweep, calls) = open_rig;
        step_until(&mut sweep, |s| s.rotation.is_open());
        sweep.restart(Preset::Quick);
        check!(calls.list() == ["open", "close"]);
        let status = sweep.status();
        check!((status.passes, status.bytes, status.preset) == (0, MAIN_WORDS * WORD, Preset::Quick));
        step_until(&mut sweep, |s| s.rotation.is_open());
        check!(calls.list() == ["open", "close", "open"], "the first pass after a restart is rotated too");
    }

    #[rstest]
    fn coverage_counts_the_reserve_once_it_has_been_tested(open_rig: (Sweep, Calls)) {
        let (mut sweep, _) = open_rig;
        sweep.rotation.reported_free = 64 << 10;
        let main = (MAIN_WORDS * WORD) as u64;
        check!(sweep.status().coverage == Coverage { tested: main, free: 64 << 10 });
        step_until(&mut sweep, |s| s.rotation.is_open());
        check!(sweep.status().coverage.tested == main, "the reserve counts when its pass is done");
        step_until(&mut sweep, |s| !s.rotation.is_open());
        let coverage = sweep.status().coverage;
        check!(coverage == Coverage { tested: main + (RESERVE_WORDS * WORD) as u64, free: 64 << 10 });
        check!(coverage.percent() == 37);
    }

    #[rstest]
    fn a_window_cut_short_does_not_count_the_reserve(open_rig: (Sweep, Calls)) {
        let (mut sweep, _) = open_rig;
        step_until(&mut sweep, |s| s.rotation.is_open());
        sweep.restart(Preset::Standard);
        check!(sweep.status().coverage.tested == (MAIN_WORDS * WORD) as u64);
    }

    /// A firmware that hands back a different set of pages than it was lent, so the sweep's memory
    /// is not the size it was before the window.
    struct Regrows;

    impl Pages for Regrows {
        fn open(&mut self, _main: &mut Claim, lend: u64) -> Option<Opened> {
            Some(Opened {
                reserve: host_blocks(&[RESERVE_WORDS]),
                lent: vec![Range::new(1 << 20, (lend / 4096) as usize)],
                free: lend,
            })
        }

        fn close(&mut self, main: &mut Claim, _reserve: Claim, _lent: &[Range]) -> Closed {
            main.absorb(host_blocks(&[512]));
            Closed { kept: 0, free: 7 << 20 }
        }
    }

    #[test]
    fn a_bit_fade_after_a_rotation_that_resized_the_memory_counts_its_wait_down_exactly() {
        use ferrite_mem::eta::Position;

        let fade = Duration::from_secs(5);
        let config = Config { fade, ..CFG };
        let mut sweep = Sweep::with_buffer(
            Some(host_blocks(&[MAIN_WORDS])),
            Kernels::uniform(Kernel::SCALAR),
            config,
            Preset::Thorough,
            None,
        )
        .with_pages(Regrows, 1 << 20);
        let mut now = Instant::from_epoch(Duration::from_secs(1));
        // Pass 0 teaches the timings, its rotation changes the memory, pass 1 runs over what came back.
        drive(&mut sweep, &mut now, |s| s.status().bytes == (MAIN_WORDS + 512) * WORD);
        check!(sweep.status().passes == 1);
        drive(&mut sweep, &mut now, |s| s.waiting_until().is_some() && s.status().test == "bit fade");
        let start = sweep.status().outlook;
        check!(start.position == Position::Wait { nth: 1, waits: 4, left: fade }, "{:?}", start.position);
        now = now + Duration::from_secs(2);
        sweep.tick(now);
        let later = sweep.status().outlook;
        check!(later.position == Position::Wait { nth: 1, waits: 4, left: Duration::from_secs(3) });
        assert!(let (Some(before), Some(after)) = (start.test_left, later.test_left));
        check!(before.saturating_sub(after) == Duration::from_secs(2), "{before:?} then {after:?}: a wait is not work");
    }
}

#[test]
fn only_aps_that_never_began_are_called_off() {
    use ferrite_mem::crew::{Crew, Job};

    let crew = Crew::default();
    let seats: Vec<Seat> = (0..4).map(|_| Seat::default()).collect();
    let tests = |_: usize| {
        vec![Test::for_share(
            TestKind::SolidBits,
            CFG,
            0,
            &Regions::new(Vec::new()).layout(),
            Span { start: 0, words: 0 },
        )]
    };
    let jobs: Vec<Job<'_, Fixed>> = seats
        .iter()
        .enumerate()
        .map(|(i, seat)| {
            Job::new(tests(i), Regions::new(Vec::new()), Kernels::uniform(Kernel::SCALAR), 4096, Fixed, &crew, seat)
        })
        .collect();
    // Seat 1 is in its job, seat 2 has ended; seats 0 and 3 never began.
    let worker = jobs[1].claim();
    assert!(let Some(_worker) = worker);
    let ended = jobs[2].claim();
    assert!(let Some(ended) = ended);
    ended.retire();
    check!(never_began(&[0, 1, 2, 3], &seats) == [0, 3]);
}

/// Step and look at `sweep` as the main loop does, on a clock that jumps to the end of a wait.
fn drive(sweep: &mut Sweep, now: &mut Instant, until: impl Fn(&Sweep) -> bool) {
    while !until(sweep) {
        sweep.tick(*now);
        if sweep.has_work(*now) {
            sweep.step(64 << 10, *now, None);
        } else if let Some(end) = sweep.waiting_until() {
            *now = end;
        }
        *now = *now + Duration::from_millis(5);
    }
    sweep.tick(*now);
}

#[test]
fn the_outlook_counts_bit_fades_waits_as_time_and_knows_the_next_pass_from_this_one() {
    use ferrite_mem::eta::{Confidence, Position};

    let fade = Duration::from_secs(5);
    let mut sweep = Sweep::with_buffer(
        Some(host_blocks(&[1024])),
        Kernels::uniform(Kernel::SCALAR),
        Config { fade, ..CFG },
        Preset::Thorough,
        None,
    );
    let mut now = Instant::from_epoch(Duration::from_secs(1));
    drive(&mut sweep, &mut now, |s| s.waiting_until().is_some() && s.status().test == "bit fade");
    let outlook = sweep.status().outlook;
    check!(outlook.position == Position::Wait { nth: 1, waits: 4, left: fade });
    check!(outlook.rate.is_none(), "no rate to report while the memory sits");
    check!(outlook.pass.is_none(), "the first pass has no priors and nothing agrees yet");
    // Two seconds into the wait.
    now = now + Duration::from_secs(2);
    sweep.tick(now);
    check!(sweep.status().outlook.position == Position::Wait { nth: 1, waits: 4, left: Duration::from_secs(3) });
    // The pass carries on through the other three waits, and the next one starts knowing its tests.
    drive(&mut sweep, &mut now, |s| s.status().passes == 1);
    let outlook = sweep.status().outlook;
    let (known, tests) = outlook.known;
    check!(known == tests && tests == sweep.status().position.1, "every test of the pass has a time now");
    assert!(let Some(left) = outlook.pass);
    check!(left.confidence == Confidence::Measured);
    check!(left.remaining >= fade * 4, "the waits alone are {:?}, and {left:?} is left", fade * 4);
}

#[test]
fn a_goal_ends_the_run_by_what_the_status_reports() {
    let mut sweep = sweep_of_words(2048);
    let goal = Goal { passes: core::num::NonZeroU64::new(2), time: None, stop_on_fault: false };
    sweep.set_goal(goal);
    let mut now = Instant::from_epoch(Duration::from_secs(1));
    drive(&mut sweep, &mut now, |s| s.status().outlook.settled >= 1);
    let o = sweep.status().outlook;
    check!(o.goal == goal && o.settled == 1);
    check!(o.goal.met(o.settled, Duration::ZERO, 0).is_none());
    drive(&mut sweep, &mut now, |s| s.status().outlook.settled >= 2);
    let o = sweep.status().outlook;
    check!(o.goal.met(o.settled, Duration::ZERO, 0).is_some());
}

/// A clock that moves on by `step` each time it is read.
struct Ticking {
    now: core::cell::Cell<Duration>,
    step: Duration,
}

impl ferrite_hal::Clock for Ticking {
    fn now(&self) -> Instant {
        let now = self.now.get();
        self.now.set(now + self.step);
        Instant::from_epoch(now)
    }
}

/// Measure a sweep over 2 MiB, working over a 1 MiB window: four chunks to fill it, four to verify.
fn measured(limits: Calibration, step: Duration) -> (Option<Measured>, Sweep) {
    let mut sweep = sweep_of_words(256 << 10);
    let clock = Ticking { now: core::cell::Cell::new(Duration::ZERO), step };
    let run = sweep.run.as_mut().unwrap();
    let kernels = run.suite.kernels();
    let mut sink = |_: Fault| {};
    let found = measure_bandwidth(run.suite.split().0, kernels, limits, &clock, None, &mut sink);
    (found, sweep)
}

const SMALL: Calibration =
    Calibration { window_bytes: 1 << 20, min_window_bytes: 1 << 20, budget: Duration::from_secs(1) };

#[test]
#[expect(clippy::used_underscore_binding, reason = "the buffer field only owns memory outside tests")]
fn bandwidth_is_the_bytes_written_and_read_over_the_time_they_took() {
    let (found, sweep) = measured(SMALL, Duration::from_millis(10));
    assert!(let Some(m) = found);
    check!(m.moved == 2 << 20, "a fill and a verify of the window");
    check!(m.took == Duration::from_millis(80));
    // One 256 KiB chunk every 10 ms.
    check!(m.bytes_per_sec == (256 << 10) * 100);
    check!(m.faults == 0);
    let base = sweep.run.as_ref().map(|r| r._buffer.blocks()[0].base().as_ptr()).unwrap();
    // SAFETY: inside the 2 MiB host block; nothing runs while it is read.
    let (first, last) = unsafe { (base.read(), base.add((1 << 17) - 1).read()) };
    check!(first == BANDWIDTH_PATTERN && last == BANDWIDTH_PATTERN, "the window is left holding the pattern");
}

#[test]
fn a_slow_machine_is_not_held_up_past_the_budget() {
    let limits = Calibration { budget: Duration::from_millis(35), ..SMALL };
    let (found, _) = measured(limits, Duration::from_millis(10));
    assert!(let Some(m) = found);
    check!(m.moved == 4 * (256 << 10), "stopped after the fourth chunk, at 40 ms");
    check!(m.bytes_per_sec == (256 << 10) * 100, "the figure is the same, from less of it");
}

#[test]
fn memory_too_small_to_be_past_the_caches_is_not_measured() {
    let limits = Calibration { min_window_bytes: 4 << 20, ..SMALL };
    check!(measured(limits, Duration::from_millis(10)).0.is_none());
}
