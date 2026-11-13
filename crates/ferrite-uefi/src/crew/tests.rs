use std::vec;
use std::vec::Vec;

use assert2::{assert, check};
use ferrite_mem::suite::{Config, Span, TestKind, shares};
use ferrite_mem::{BLOCK_BYTES, BLOCK_WORDS, Kernel};

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
    let mut region = unsafe { Region::from_raw(backing.as_mut_ptr().add(offset), WORDS) }.unwrap();
    let kind = TestKind::BlockMove;
    let parts = core::num::NonZeroUsize::new(2).unwrap();
    let split: Vec<(Span, Region<'_>)> = shares(&mut region, kind, &CFG, parts).collect();
    assert!(split.len() == 2);
    let crew = Crew::default();
    let seats = [Seat::default(), Seat::default()];
    let jobs: Vec<Job<'_, Fixed>> = split
        .into_iter()
        .zip(&seats)
        .map(|((span, region), seat)| {
            let test = Test::for_share(kind, CFG, 0, WORDS, span);
            Job::new(test, region, Kernels::uniform(Kernel::SCALAR), 64 << 10, Fixed, &crew, seat)
        })
        .collect();
    let mut workers: Vec<_> = jobs.iter().map(|j| j.claim().unwrap()).collect();
    check!(jobs[0].claim().is_none(), "a job has one worker");

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
    check!(seats.iter().all(|s| s.state() == SeatState::Done && s.faults() == 0));
    check!(seats.iter().map(Seat::done).sum::<u64>() > 0);
}

#[test]
fn a_stop_ends_every_job_at_its_next_turn() {
    let (mut backing, offset) = buffer();
    // SAFETY: as above.
    let mut region = unsafe { Region::from_raw(backing.as_mut_ptr().add(offset), WORDS) }.unwrap();
    let kind = TestKind::SolidBits;
    let crew = Crew::default();
    let seat = Seat::default();
    let test = Test::for_share(kind, CFG, 0, WORDS, Span { start: 0, words: WORDS });
    let job = Job::new(test, region.sub(0, WORDS), Kernels::uniform(Kernel::SCALAR), 1 << 10, Fixed, &crew, &seat);
    let mut worker = job.claim().unwrap();
    check!(worker.turn(None) == Turn::Worked);
    crew.stop();
    check!(worker.turn(None) == Turn::Stopped);
    check!(seat.state() == SeatState::Stopped);
    check!(crew.coordinate([&seat]));
}
