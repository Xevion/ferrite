use std::vec;
use std::vec::Vec;

use assert2::{assert, check};
use ferrite_mem::suite::{Config, Span, TestKind, shares};
use ferrite_mem::{BLOCK_BYTES, BLOCK_WORDS, Kernel, Region};

use super::*;

/// A clock that stays at one instant.
#[derive(Clone, Copy)]
struct Fixed;

impl Clock for Fixed {
    fn now(&self) -> Instant {
        Instant::from_epoch(Duration::from_millis(7))
    }
}

const WORDS: usize = 2048;
/// Four 512-word block-move windows, so two shares hold two windows each.
const CFG: Config = Config { move_iterations: 2, move_window_bytes: 4 << 10, ..Config::DEFAULT };

/// A 64-byte-aligned host buffer.
fn buffer() -> (Vec<u64>, usize) {
    let backing = vec![0; WORDS + BLOCK_WORDS];
    let offset = backing.as_ptr().align_offset(BLOCK_BYTES);
    (backing, offset)
}

/// Turn `worker` until it stops at a gate or ends; the turn that stopped it.
fn until_stopped(worker: &mut Worker<'_, '_, Fixed>) -> Turn {
    loop {
        match worker.turn(None) {
            Turn::Worked => {},
            turn => return turn,
        }
    }
}

#[test]
fn gates_hold_every_core_until_all_arrive_except_between_move_windows() {
    let (mut backing, offset) = buffer();
    // SAFETY: the buffer is ours, and nothing else touches it while the region lives.
    let region = unsafe { Region::from_raw(backing.as_mut_ptr().add(offset), WORDS) }.unwrap();
    let mut regions = Regions::from(region);
    let layout = regions.layout();
    let kind = TestKind::BlockMove;
    let parts = core::num::NonZeroUsize::new(2).unwrap();
    let split: Vec<(Span, Regions<'_>)> = shares(&mut regions, kind, &CFG, parts).collect();
    assert!(split.len() == 2);
    let crew = Crew::default();
    let seats = [Seat::default(), Seat::default()];
    let jobs: Vec<Job<'_, Fixed>> = split
        .into_iter()
        .zip(&seats)
        .map(|((span, regions), seat)| {
            let test = Test::for_share(kind, CFG, 0, &layout, span);
            Job::new(vec![test], regions, Kernels::uniform(Kernel::SCALAR), 64 << 10, Fixed, &crew, seat)
        })
        .collect();
    let mut workers: Vec<_> = jobs.iter().map(|j| j.claim().unwrap()).collect();
    check!(jobs[0].claim().is_none(), "a job has one worker");
    crew.begin_round();
    for w in &mut workers {
        w.begin(crew.round());
    }

    // MoveInit, four windows (two per share), then VerifyPairs: gates at phases 1 and 5 only.
    check!(until_stopped(&mut workers[0]) == Turn::Gate);
    check!(seats[0].gate.load(Ordering::Relaxed) == 1);
    check!(!crew.coordinate(&seats), "core 1 has not arrived");
    check!(workers[0].turn(None) == Turn::Gate, "still held");
    check!(until_stopped(&mut workers[1]) == Turn::Gate);
    check!(!crew.coordinate(&seats));
    check!(crew.open.load(Ordering::Relaxed) == 1);

    check!(until_stopped(&mut workers[0]) == Turn::Gate);
    check!(seats[0].gate.load(Ordering::Relaxed) == 5, "the windows ran without a gate between them");
    check!(!crew.coordinate(&seats), "core 1 is still in its windows");
    check!(crew.open.load(Ordering::Relaxed) == 1);
    check!(until_stopped(&mut workers[1]) == Turn::Gate);
    check!(!crew.coordinate(&seats));
    for worker in &mut workers {
        check!(until_stopped(worker) == Turn::Done);
    }
    check!(crew.coordinate(&seats), "every seat ended");
    check!(seats.iter().all(|s| s.settled(1) && s.faults() == 0));
    check!(seats.iter().map(Seat::done).sum::<u64>() > 0);
}

#[test]
fn a_stop_ends_every_job_at_its_next_turn() {
    let (mut backing, offset) = buffer();
    // SAFETY: as above.
    let region = unsafe { Region::from_raw(backing.as_mut_ptr().add(offset), WORDS) }.unwrap();
    let kind = TestKind::SolidBits;
    let crew = Crew::default();
    let seat = Seat::default();
    let regions = Regions::from(region);
    let test = Test::for_share(kind, CFG, 0, &regions.layout(), Span { start: 0, words: WORDS });
    let job = Job::new(vec![test], regions, Kernels::uniform(Kernel::SCALAR), 1 << 10, Fixed, &crew, &seat);
    let mut worker = job.claim().unwrap();
    crew.begin_round();
    worker.begin(crew.round());
    check!(worker.turn(None) == Turn::Worked);
    crew.stop();
    check!(worker.turn(None) == Turn::Stopped);
    check!(seat.state() == SeatState::Stopped);
    check!(crew.coordinate([&seat]));
}

/// Jobs of `kinds` over three shares of `ptr`'s `WORDS` words, one per core.
fn run_of<'a>(
    kinds: &[TestKind],
    regions: &'a mut Regions<'_>,
    crew: &'a Crew,
    seats: &'a [Seat],
) -> Vec<Job<'a, Fixed>> {
    let layout = regions.layout();
    let parts = core::num::NonZeroUsize::new(seats.len()).unwrap();
    shares(regions, kinds[0], &CFG, parts)
        .zip(seats)
        .map(|((span, regions), seat)| {
            let tests = kinds.iter().map(|&k| Test::for_share(k, CFG, 0, &layout, span)).collect();
            Job::new(tests, regions, Kernels::uniform(Kernel::SCALAR), 16 << 10, Fixed, crew, seat)
        })
        .collect()
}

/// Wait until every seat has finished `round`, working the BSP's own job and the gates meanwhile.
fn finish_round<'a>(crew: &Crew, seats: &[Seat], own: &mut Worker<'a, 'a, Fixed>) {
    let mut own_done = false;
    while !crew.coordinate(seats) || !own_done {
        own_done |= matches!(own.turn(None), Turn::Done);
    }
}

#[test]
fn cores_move_from_one_test_to_the_next_only_when_the_bsp_begins_the_round() {
    let (mut backing, offset) = buffer();
    // SAFETY: the buffer is ours, and is read below only while every core waits for the next round.
    let ptr = unsafe { backing.as_mut_ptr().add(offset) };
    // SAFETY: as above.
    let region = unsafe { Region::from_raw(ptr, WORDS) }.unwrap();
    let mut regions = Regions::from(region);
    let (crew, seats) = (Crew::default(), [Seat::default(), Seat::default(), Seat::default()]);
    let kinds = [TestKind::SolidBits, TestKind::Checkerboard];
    let jobs = run_of(&kinds, &mut regions, &crew, &seats);
    // SAFETY: only called while every core waits, and `ptr` covers `WORDS` words.
    let words = |ptr: *mut u64| (0..WORDS).map(|w| unsafe { ptr.add(w).read() }).collect::<Vec<u64>>();

    let ends = std::thread::scope(|scope| {
        let aps: Vec<_> = jobs[1..].iter().map(|j| scope.spawn(|| j.claim().unwrap().serve(None))).collect();
        let mut own = jobs[0].claim().unwrap();
        // The first test ends with the whole memory filled with ones.
        crew.begin_round();
        own.begin(crew.round());
        finish_round(&crew, &seats, &mut own);
        let (done, total) = (seats.iter().map(Seat::done).sum::<u64>(), 4 * WORDS as u64);
        check!(done == total, "round 1 covered every word: {done} of {total}");
        check!(words(ptr).iter().all(|&w| w == !0));
        std::thread::sleep(Duration::from_millis(30));
        check!(words(ptr).iter().all(|&w| w == !0), "no core starts the next test on its own");
        check!(crew.coordinate(&seats), "settled for round 1");

        seats.iter().for_each(Seat::reset);
        crew.begin_round();
        own.begin(crew.round());
        check!(seats.iter().all(|s| s.done() == 0), "a round starts from nothing done");
        finish_round(&crew, &seats, &mut own);
        check!(words(ptr).iter().all(|&w| w == 0x5555_5555_5555_5555), "round 2 ran the second test");
        crew.close();
        own.retire();
        aps.into_iter().map(|ap| ap.join().unwrap()).collect::<Vec<_>>()
    });
    check!(ends == [SeatState::Done, SeatState::Done]);
    check!(seats.iter().all(|s| s.faults() == 0 && s.settled(2)));
}

#[test]
fn a_stop_between_tests_releases_cores_waiting_for_the_next_round() {
    let (mut backing, offset) = buffer();
    // SAFETY: the buffer is ours, and nothing else touches it while the region lives.
    let region = unsafe { Region::from_raw(backing.as_mut_ptr().add(offset), WORDS) }.unwrap();
    let mut regions = Regions::from(region);
    let (crew, seats) = (Crew::default(), [Seat::default(), Seat::default(), Seat::default()]);
    let jobs = run_of(&[TestKind::SolidBits, TestKind::Checkerboard], &mut regions, &crew, &seats);
    let ends = std::thread::scope(|scope| {
        let aps: Vec<_> = jobs[1..].iter().map(|j| scope.spawn(|| j.claim().unwrap().serve(None))).collect();
        let mut own = jobs[0].claim().unwrap();
        crew.begin_round();
        own.begin(crew.round());
        finish_round(&crew, &seats, &mut own);
        crew.stop();
        check!(own.turn(None) == Turn::Stopped);
        aps.into_iter().map(|ap| ap.join().unwrap()).collect::<Vec<_>>()
    });
    check!(ends == [SeatState::Stopped, SeatState::Stopped]);
    check!(seats[1..].iter().all(|s| s.state() == SeatState::Stopped));
}

#[test]
fn a_stop_in_the_middle_of_a_test_ends_every_core_before_it_finishes() {
    let (mut backing, offset) = buffer();
    // SAFETY: as above.
    let region = unsafe { Region::from_raw(backing.as_mut_ptr().add(offset), WORDS) }.unwrap();
    let mut regions = Regions::from(region);
    let (crew, seats) = (Crew::default(), [Seat::default(), Seat::default(), Seat::default()]);
    let jobs = run_of(&[TestKind::MovingInversions64Bit], &mut regions, &crew, &seats);
    let ends = std::thread::scope(|scope| {
        let aps: Vec<_> = jobs[1..].iter().map(|j| scope.spawn(|| j.claim().unwrap().serve(None))).collect();
        let mut own = jobs[0].claim().unwrap();
        crew.begin_round();
        own.begin(crew.round());
        // A few turns in, nobody can be done: the test is hundreds of phases.
        for _ in 0..8 {
            own.turn(None);
        }
        crew.stop();
        check!(own.turn(None) == Turn::Stopped);
        aps.into_iter().map(|ap| ap.join().unwrap()).collect::<Vec<_>>()
    });
    check!(ends == [SeatState::Stopped, SeatState::Stopped]);
    check!(seats.iter().all(|s| !s.settled(1) || s.state() == SeatState::Stopped));
}
