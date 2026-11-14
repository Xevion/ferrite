use std::vec;
use std::vec::Vec;

use crate::suite::{Config, Span, TestKind, shares};
use crate::{BLOCK_BYTES, BLOCK_WORDS, Kernel, Region};
use assert2::{assert, check};

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

#[test]
fn own_address_is_checked_only_once_every_core_has_written() {
    let (mut backing, offset) = buffer();
    // SAFETY: the buffer is ours; the test writes through `ptr` only while every worker waits at a gate.
    let ptr = unsafe { backing.as_mut_ptr().add(offset) };
    // SAFETY: as above.
    let region = unsafe { Region::from_raw(ptr, WORDS) }.unwrap();
    let mut regions = Regions::from(region);
    let (crew, seats) = (Crew::default(), [Seat::default(), Seat::default()]);
    let jobs = run_of(&[TestKind::OwnAddress], &mut regions, &crew, &seats);
    let mut workers: Vec<_> = jobs.iter().map(|j| j.claim().unwrap()).collect();
    crew.begin_round();
    for w in &mut workers {
        w.begin(crew.round());
    }

    // Both cores have written every address of their share and wait to check them.
    for w in &mut workers {
        check!(until_stopped(w) == Turn::Gate);
    }
    check!(seats.iter().all(|s| s.faults() == 0), "nothing is checked before every core has written");
    // A write meant for word 1500, in the second share, also landed on word 300, in the first.
    let (victim, source) = (300, 1500);
    // SAFETY: both words are in the buffer and no worker is running.
    let (expected, actual) = unsafe { (ptr.add(victim).read(), ptr.add(source).read()) };
    // SAFETY: as above.
    unsafe { ptr.add(victim).write(actual) };
    check!(!crew.coordinate(&seats), "the gate opens once both cores are there");
    for w in &mut workers {
        check!(until_stopped(w) == Turn::Gate, "both check their shares, then wait to write the complement");
    }
    let kept: Vec<Fault> = seats[0].kept_from(0).collect();
    let addr = CpuAddr::of(ptr.wrapping_add(victim));
    check!(kept == [Fault { addr, expected, actual }]);
    check!(seats[1].faults() == 0);
}

#[test]
fn split_own_address_and_march_run_clean_across_threads() {
    let (mut backing, offset) = buffer();
    // SAFETY: the buffer is ours, and is read below only while every core waits for the next round.
    let ptr = unsafe { backing.as_mut_ptr().add(offset) };
    // SAFETY: as above.
    let region = unsafe { Region::from_raw(ptr, WORDS) }.unwrap();
    let mut regions = Regions::from(region);
    let (crew, seats) = (Crew::default(), [Seat::default(), Seat::default(), Seat::default()]);
    let kinds = [TestKind::OwnAddress, TestKind::MarchCMinus];
    let jobs = run_of(&kinds, &mut regions, &crew, &seats);
    let ends = std::thread::scope(|scope| {
        let aps: Vec<_> = jobs[1..].iter().map(|j| scope.spawn(|| j.claim().unwrap().serve(None))).collect();
        let mut own = jobs[0].claim().unwrap();
        for round in 1..=kinds.len() {
            if round > 1 {
                seats.iter().for_each(Seat::reset);
            }
            crew.begin_round();
            own.begin(crew.round());
            finish_round(&crew, &seats, &mut own);
            check!(seats.iter().all(|s| s.faults() == 0), "round {round}");
        }
        // SAFETY: every core waits for a round that never comes.
        check!((0..WORDS).all(|w| unsafe { ptr.add(w).read() } == 0), "march c- leaves zeros");
        crew.close();
        own.retire();
        aps.into_iter().map(|ap| ap.join().unwrap()).collect::<Vec<_>>()
    });
    check!(ends == [SeatState::Done, SeatState::Done]);
}

/// A clock that follows the host's.
struct Wall(std::time::Instant);

impl Clock for Wall {
    fn now(&self) -> Instant {
        Instant::from_epoch(self.0.elapsed())
    }
}

#[test]
fn the_start_line_releases_cores_once_all_have_arrived_and_abandons_if_one_never_does() {
    let clock = Wall(std::time::Instant::now());
    let patience = Duration::from_secs(5);
    let line = StartLine::default();
    let go = std::thread::scope(|scope| {
        let aps: Vec<_> = (0..2).map(|_| scope.spawn(|| line.arrive_and_wait(&clock, patience))).collect();
        check!(line.gather(2, &clock, patience, &mut || {}).is_some());
        aps.into_iter().map(|ap| ap.join().unwrap()).collect::<Vec<_>>()
    });
    check!(go == [Start::Go, Start::Go]);

    let line = StartLine::default();
    let gone = std::thread::scope(|scope| {
        let ap = scope.spawn(|| line.arrive_and_wait(&clock, patience));
        check!(line.gather(2, &clock, Duration::from_millis(20), &mut || {}).is_none(), "one core never arrives");
        ap.join().unwrap()
    });
    check!(gone == Start::Abandoned);
    check!(line.arrive_and_wait(&clock, patience) == Start::Abandoned, "a late core is sent home too");
}

/// A core whose features the test sets.
struct Core {
    sse: bool,
    wide: bool,
}

impl WideGuard for () {
    fn wide(&self, f: &mut dyn FnMut()) {
        f();
    }
}

impl Platform for Core {
    type Wide = ();

    fn sse_ready(&self) -> bool {
        self.sse
    }

    fn enable_wide(&self) -> Option<()> {
        Some(())
    }

    fn tier_usable(&self, tier: Tier) -> bool {
        self.wide || !tier.is_wide()
    }
}

#[test]
fn an_ap_enters_its_job_only_if_the_core_can_run_it() {
    let clock = Wall(std::time::Instant::now());
    let narrow = [Kernel::SCALAR];
    let enter = |core: &Core, kernels: &[Kernel], line: &StartLine| {
        let (seat, taken) = (Seat::default(), AtomicBool::new(false));
        let entered = enter_ap(core, &seat, &taken, kernels, line, &clock).is_some();
        (entered, seat.state())
    };
    let released = StartLine::default();
    released.release();
    check!(enter(&Core { sse: true, wide: false }, &narrow, &released) == (true, SeatState::Running));
    check!(enter(&Core { sse: false, wide: true }, &narrow, &released) == (false, SeatState::Failed(Failure::NoSse)));
    if let Some(wide) = crate::tests::kernels().into_iter().find(|k| k.tier().is_wide()) {
        let core = Core { sse: true, wide: false };
        check!(enter(&core, &[wide], &released) == (false, SeatState::Failed(Failure::Tier)));
    }
    let line = StartLine::default();
    line.abandon();
    check!(enter(&Core { sse: true, wide: true }, &narrow, &line) == (false, SeatState::Stopped));

    let (seat, taken) = (Seat::default(), AtomicBool::new(true));
    let core = Core { sse: true, wide: true };
    check!(enter_ap(&core, &seat, &taken, &narrow, &released, &clock).is_none());
    check!(seat.state() == SeatState::Failed(Failure::Taken));
}
