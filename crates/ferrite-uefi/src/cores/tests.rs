use std::vec;
use std::vec::Vec;

use assert2::{assert, check};
use ferrite_mem::{BLOCK_BYTES, BLOCK_WORDS};
use rstest::rstest;

use super::*;

/// A clock that stays at one instant.
#[derive(Clone, Copy)]
struct Fixed(Instant);

impl Clock for Fixed {
    fn now(&self) -> Instant {
        self.0
    }
}

const AT: Instant = Instant::from_epoch(Duration::from_millis(7));

/// A 64-byte-aligned host buffer of `words` words.
struct Buf {
    backing: Vec<u64>,
    offset: usize,
    words: usize,
}

impl Buf {
    fn new(words: usize) -> Self {
        let backing = vec![0; words + BLOCK_WORDS];
        let offset = backing.as_ptr().align_offset(BLOCK_BYTES);
        Self { backing, offset, words }
    }

    fn region(&mut self) -> Region<'_> {
        // SAFETY: the buffer is ours, and no reference into it is used while the region lives.
        unsafe { Region::from_raw(self.backing.as_mut_ptr().add(self.offset), self.words) }.unwrap()
    }

    fn word_mut(&mut self, word: usize) -> &mut u64 {
        &mut self.backing[self.offset + word]
    }

    fn addr(&self, word: usize) -> usize {
        (&raw const self.backing[self.offset + word]).addr()
    }
}

fn parts(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}

/// Run `work` over `buf` split `cores` ways, each slice through `run_here` as the BSP would.
fn run(buf: &mut Buf, cores: usize, work: Work, passes: u32) -> (Summary, Vec<CoreReport>) {
    let spec = Spec { kernel: Kernel::SCALAR, work, passes, chunk_words: 3 * BLOCK_WORDS, clock: Fixed(AT) };
    let line = StartLine::default();
    let slots: Vec<Slot> = (0..cores).map(|_| Slot::default()).collect();
    let mut region = buf.region();
    with_split(&mut region, parts(cores), work == Work::Copy, |slices| {
        for (slice, slot) in slices.into_iter().zip(&slots) {
            CoreJob::new(spec, slice, &line, slot).run_here(None, &mut || {});
        }
    });
    let reports: Vec<CoreReport> = slots.iter().map(Slot::report).collect();
    (Summary::of(Instant::from_epoch(Duration::ZERO), &reports), reports)
}

#[rstest]
#[case::one(16, 1)]
#[case::even(16, 4)]
#[case::uneven(16, 3)]
#[case::large_prime(1031, 7)]
#[case::more_parts_than_blocks(5, 8)]
fn slices_tile_the_region_in_whole_aligned_blocks(#[case] blocks: usize, #[case] cores: usize) {
    let mut buf = Buf::new(blocks * BLOCK_WORDS);
    let mut region = buf.region();
    let base = region.base().get();
    let total = region.bytes();
    with_split(&mut region, parts(cores), false, |slices| {
        assert!(!slices.is_empty());
        assert!(slices.len() <= cores);
        let mut next = base;
        for s in &slices {
            let r = s.region();
            check!(r.base().get() == next);
            check!(r.base().get() % BLOCK_BYTES == 0);
            check!(r.words() % BLOCK_WORDS == 0);
            check!(r.words() > 0);
            check!(s.block * BLOCK_BYTES as u64 == (next - base) as u64);
            next += r.bytes();
        }
        check!(next - base == total);
        let sizes: Vec<usize> = slices.iter().map(|s| s.region().words()).collect();
        check!(sizes[..sizes.len() - 1].iter().all(|&w| w == sizes[0]));
    });
}

#[rstest]
#[case::even(16, 4)]
#[case::odd_blocks(17, 3)]
fn copy_slices_pair_the_lower_half_with_the_upper(#[case] blocks: usize, #[case] cores: usize) {
    let mut buf = Buf::new(blocks * BLOCK_WORDS);
    let mut region = buf.region();
    let base = region.base().get();
    let half = blocks / 2 * BLOCK_BYTES;
    with_split(&mut region, parts(cores), true, |slices| {
        let mut next = base;
        for s in &slices {
            let src = s.source().unwrap();
            check!(src.base().get() == next);
            check!(s.region().base().get() == next + half);
            check!(s.region().words() == src.words());
            next += src.bytes();
        }
        check!(next - base == half);
    });
}

#[test]
fn faults_are_counted_and_kept_per_core() {
    const VALUE: u64 = 0x1234_5678_9abc_def0;
    let mut buf = Buf::new(32 * BLOCK_WORDS);
    let (summary, _) = run(&mut buf, 4, Work::Fill { value: VALUE }, 1);
    assert!(summary.complete());
    // Slices are 8 blocks (64 words): two faults in core 1's slice, one in core 3's.
    let bad = [70, 100, 200];
    for &w in &bad {
        *buf.word_mut(w) ^= 1;
    }
    let (summary, reports) = run(&mut buf, 4, Work::Verify { value: VALUE }, 2);
    check!(summary.complete());
    check!(summary.faults == 6);
    check!(summary.bytes == 2 * 32 * BLOCK_BYTES as u64);
    let counts: Vec<u64> = reports.iter().map(|r| r.faults).collect();
    check!(counts == [0, 4, 0, 2]);
    let kept: Vec<usize> = reports[1].first.iter().map(|f| f.addr.get()).collect();
    check!(kept == [buf.addr(70), buf.addr(100), buf.addr(70), buf.addr(100)]);
    check!(reports[3].first[0] == Fault { addr: reports[3].first[0].addr, expected: VALUE, actual: VALUE ^ 1 });
    check!(reports[3].first[0].addr.get() == buf.addr(200));
}

#[test]
fn a_split_prsg_fill_is_one_sequence_over_the_region() {
    const SEED: u64 = 42;
    let mut buf = Buf::new(40 * BLOCK_WORDS);
    let (summary, _) = run(&mut buf, 3, Work::FillPrsg { seed: SEED }, 1);
    assert!(summary.complete());
    let mut seen = 0;
    let mut sink = |_| seen += 1;
    let mut faults = Faults::new(1, &mut sink);
    ferrite_mem::verify_prsg(Kernel::SCALAR, &buf.region(), &mut Prsg::new(SEED), 0, &mut faults);
    check!(faults.seen() == 0);
    let (summary, _) = run(&mut buf, 5, Work::VerifyPrsg { seed: SEED }, 1);
    check!(summary.faults == 0);
}

#[test]
fn even_inversion_passes_leave_the_pattern_and_copy_mirrors_the_halves() {
    const P: u64 = 0x0ff0_0ff0_a5a5_5a5a;
    let mut buf = Buf::new(16 * BLOCK_WORDS);
    run(&mut buf, 2, Work::Fill { value: P }, 1);
    let (summary, _) = run(&mut buf, 3, Work::Invert { pattern: P }, 4);
    check!(summary.complete());
    check!(summary.faults == 0);
    *buf.word_mut(5) = 9;
    let (summary, _) = run(&mut buf, 3, Work::Copy, 1);
    check!(summary.bytes == 16 * BLOCK_BYTES as u64);
    check!(*buf.word_mut(8 * BLOCK_WORDS + 5) == 9);
    check!(*buf.word_mut(8 * BLOCK_WORDS + 6) == P);
}

#[test]
fn a_job_runs_once() {
    let mut buf = Buf::new(8 * BLOCK_WORDS);
    let spec =
        Spec { kernel: Kernel::SCALAR, work: Work::Fill { value: 1 }, passes: 1, chunk_words: 64, clock: Fixed(AT) };
    let (line, slot) = (StartLine::default(), Slot::default());
    let mut region = buf.region();
    with_split(&mut region, parts(1), false, |slices| {
        let job = CoreJob::new(spec, slices.into_iter().next().unwrap(), &line, &slot);
        job.run_here(None, &mut || {});
        check!(slot.report().state == CoreState::Done);
        job.run_here(None, &mut || {});
        check!(slot.report().state == CoreState::Failed(Failure::Taken));
    });
}

#[test]
fn the_summary_spans_to_the_last_core_and_counts_failures() {
    let at = |ms| Some(Instant::from_epoch(Duration::from_millis(ms)));
    let report = |state, bytes, faults, ended, stack_bytes| CoreReport {
        state,
        passes: 1,
        bytes,
        faults,
        ended,
        stack_bytes,
        first: Vec::new(),
    };
    let reports = [
        report(CoreState::Done, 100, 0, at(30), None),
        report(CoreState::Done, 100, 3, at(50), Some(900)),
        report(CoreState::Running, 40, 1, None, Some(1200)),
    ];
    let summary = Summary::of(Instant::from_epoch(Duration::from_millis(10)), &reports);
    check!(summary.span == Duration::from_millis(40));
    check!(summary.bytes == 240);
    check!(summary.faults == 4);
    check!(summary.done == 2);
    check!(!summary.complete());
    check!(summary.stack_bytes == Some(1200));
}
