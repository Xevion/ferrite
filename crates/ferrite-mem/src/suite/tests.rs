use core::num::NonZeroUsize;
use std::vec::Vec;

use assert2::{assert, check};
use ferrite_hal::Instant;
use rstest::rstest;

use super::*;
use crate::Caps;
use crate::fault::Fault;
use crate::kernels::{Cells, address_walk};
use crate::ops::{Inversion, Order, fill_prsg, fill_rotating, invert_prsg, invert_rotating};
use crate::prsg::Prsg;
use crate::region::BLOCK_WORDS;
use crate::rotating::Rotating;
use crate::tests::{Buf, kernels};

const WORDS: usize = 2048;
/// Memory in separate host buffers, one region each, so addresses jump between regions.
struct Mem {
    bufs: Vec<Buf>,
}

impl Mem {
    fn new(layout: &[usize]) -> Self {
        Self { bufs: layout.iter().map(|&words| Buf::new(words)).collect() }
    }

    fn regions(&mut self) -> Regions<'_> {
        Regions::new(self.bufs.iter_mut().map(Buf::region))
    }
}

/// The same 2048 words as one region and as three of whole move windows each, so a test has the
/// same phases over both.
const LAYOUTS: [&[usize]; 2] = [&[WORDS], &[512, 1024, 512]];
/// Three regions, none a whole number of move windows, so the last window of each is short.
const RAGGED: &[usize] = &[640, 768, 640];
const ALL_LAYOUTS: [&[usize]; 3] = [LAYOUTS[0], LAYOUTS[1], RAGGED];
/// Seven blocks, which divides no span, so every phase ends on a short chunk.
const CHUNK_BYTES: usize = 7 * 64;

/// Small counts and windows, so every phase runs and the windowed tests have several windows.
const CFG: Config = Config {
    fixed_iterations: 1,
    byte_iterations: 1,
    random_rounds: 4,
    shifting_iterations: 1,
    modulo_rounds: 1,
    move_iterations: 3,
    bus_iterations: 3,
    fade: Duration::from_millis(1),
    address_window_bytes: 4 << 10,
    move_window_bytes: 4 << 10,
    ..Config::DEFAULT
};

/// A fault injected between two phases, by writing the memory behind the test's back.
#[derive(Clone, Copy, Debug)]
enum Injection {
    /// The word reads back with `mask` flipped.
    Flip { word: usize, mask: u64 },
    /// The victim's bits under `mask` flip where the aggressor holds ones.
    Couple { aggressor: usize, victim: usize, mask: u64 },
    /// A write meant for `from` also landed on `to`.
    Alias { from: usize, to: usize },
}

impl Injection {
    /// Corrupt `region`, returning the fault the next read of the victim must report.
    fn apply(self, region: &mut Regions<'_>) -> Fault {
        let (victim, actual) = match self {
            Self::Flip { word, mask } => (word, region.peek(word) ^ mask),
            Self::Couple { aggressor, victim, mask } => (victim, region.peek(victim) ^ (region.peek(aggressor) & mask)),
            Self::Alias { from, to } => (to, region.peek(from)),
        };
        let expected = region.peek(victim);
        assert!(actual != expected, "{self:?} changes nothing");
        region.poke(victim, actual);
        Fault { addr: region.addr(victim), expected, actual }
    }
}

/// Run `kind` to the end over `buf`, injecting after phase `after`; the faults reported, and the
/// one the injection must cause.
fn run(
    kind: TestKind,
    kernels: Kernels,
    mem: &mut Mem,
    inject: Option<(usize, Injection)>,
) -> (Vec<Fault>, Option<Fault>) {
    let mut reported = Vec::new();
    let mut sink = |f| reported.push(f);
    let mut faults = Faults::new(64, &mut sink);
    let mut region = mem.regions();
    let mut test = Test::new(kind, CFG, 0, &region);
    let mut now = Instant::default();
    let mut want = None;
    loop {
        let phase = test.phase();
        let progress = test.step(&mut region, &kernels, CHUNK_BYTES, now, &mut faults);
        if let Some((after, injection)) = inject
            && after == phase
            && test.phase() == phase + 1
        {
            want = Some(injection.apply(&mut region));
        }
        match progress {
            Progress::Running => {},
            Progress::Waiting { until } => now = until,
            Progress::Done => break,
        }
    }
    check!(test.done() == test.total(), "{kind:?}");
    (reported, want)
}

#[test]
fn every_test_passes_clean_memory_on_every_tier() {
    for layout in ALL_LAYOUTS {
        for &kind in TestKind::ALL {
            for kernel in kernels() {
                let (faults, _) = run(kind, Kernels::uniform(kernel), &mut Mem::new(layout), None);
                check!(faults.is_empty(), "{kind:?} on {kernel} over {layout:?}");
            }
        }
    }
}

#[test]
fn every_test_passes_clean_memory_with_the_mixed_kernels() {
    // SAFETY: the flags come from the host's own feature detection.
    let caps = unsafe { Caps::new(std::is_x86_feature_detected!("avx2"), std::is_x86_feature_detected!("avx512f")) };
    for layout in ALL_LAYOUTS {
        for &kind in TestKind::ALL {
            let (faults, _) = run(kind, Kernels::for_caps(caps), &mut Mem::new(layout), None);
            check!(faults.is_empty(), "{kind:?} over {layout:?}");
        }
    }
}

use Injection::{Alias, Couple, Flip};

#[rstest]
#[case::solid_stuck(TestKind::SolidBits, 0, Flip { word: 37, mask: 1 << 9 })]
#[case::solid_stuck_high(TestKind::SolidBits, 2, Flip { word: WORDS - 1, mask: 1 << 63 })]
#[case::checkerboard_coupling(TestKind::Checkerboard, 0, Couple { aggressor: 101, victim: 100, mask: 0xff })]
#[case::walking_ones_stuck(TestKind::WalkingOnes, 0, Flip { word: 9, mask: 1 << 40 })]
#[case::walking_ones_coupling(TestKind::WalkingOnes, 2, Couple { aggressor: 700, victim: 701, mask: !0 })]
#[case::walking_zeros_coupling(TestKind::WalkingZeros, 124, Couple { aggressor: 3, victim: 4, mask: 1 << 2 })]
#[case::march_stuck(TestKind::MarchCMinus, 0, Flip { word: 513, mask: 1 << 3 })]
#[case::march_transition(TestKind::MarchCMinus, 1, Flip { word: 64, mask: !0 })]
#[case::march_coupling(TestKind::MarchCMinus, 3, Couple { aggressor: 900, victim: 899, mask: 1 << 31 })]
#[case::own_address_alias(TestKind::OwnAddress, 0, Alias { from: 1000, to: 300 })]
#[case::own_address_complement_alias(TestKind::OwnAddress, 2, Alias { from: 8, to: 2040 })]
#[case::own_address_window_alias(TestKind::OwnAddressWindow, 2, Alias { from: 512, to: 600 })]
#[case::inversions_stuck(TestKind::MovingInversions, 0, Flip { word: 17, mask: 1 << 1 })]
#[case::inversions_transition(TestKind::MovingInversions, 1, Flip { word: 1500, mask: !0 })]
#[case::inversions_coupling(TestKind::MovingInversions, 1, Couple { aggressor: 50, victim: 51, mask: 1 << 7 })]
#[case::inversions_stuck_in_the_last_pass(TestKind::MovingInversions, 2, Flip { word: 900, mask: 1 << 44 })]
#[case::byte_inversions_stuck(TestKind::MovingInversions8Bit, 3, Flip { word: 2001, mask: 1 << 6 })]
#[case::byte_inversions_transition(TestKind::MovingInversions8Bit, 4, Flip { word: 2001, mask: !0 })]
#[case::random_inversions_stuck(TestKind::MovingInversionsRandom, 0, Flip { word: 333, mask: 1 << 50 })]
#[case::random_inversions_transition(TestKind::MovingInversionsRandom, 1, Flip { word: 1024, mask: !0 })]
#[case::random_inversions_splat_round(TestKind::MovingInversionsRandom, 6, Flip { word: 5, mask: 1 << 12 })]
#[case::random_inversions_stuck_before_the_merged_pass(TestKind::MovingInversionsRandom, 1, Flip { word: 700, mask: 1 << 9 })]
#[case::random_inversions_stuck_after_the_merged_pass(TestKind::MovingInversionsRandom, 2, Flip { word: 800, mask: 1 << 31 })]
#[case::random_inversions_alias_before_the_merged_pass(TestKind::MovingInversionsRandom, 1, Alias { from: 100, to: 1900 })]
#[case::random_inversions_alias_after_the_merged_pass(TestKind::MovingInversionsRandom, 2, Alias { from: 100, to: 1900 })]
#[case::byte_inversions_stuck_after_the_merged_pass(TestKind::MovingInversions8Bit, 3, Flip { word: 1234, mask: 1 << 2 })]
#[case::shifting_inversions_stuck(TestKind::MovingInversions64Bit, 0, Flip { word: 63, mask: 1 })]
#[case::shifting_inversions_transition(TestKind::MovingInversions64Bit, 4, Flip { word: 1999, mask: !0 })]
#[case::shifting_inversions_coupling(TestKind::MovingInversions64Bit, 1, Couple { aggressor: 2, victim: 1, mask: !0 })]
#[case::shifting_inversions_stuck_after_the_merged_pass(TestKind::MovingInversions64Bit, 3, Flip { word: 77, mask: 1 << 20 })]
#[case::shifting_inversions_alias_after_the_merged_pass(TestKind::MovingInversions64Bit, 3, Alias { from: 8, to: 1500 })]
#[case::modulo_stuck(TestKind::Modulo20, 0, Flip { word: 40, mask: 1 << 33 })]
#[case::modulo_stuck_after_the_single_fill_except(TestKind::Modulo20, 1, Flip { word: 40, mask: 1 << 33 })]
#[case::modulo_coupling(TestKind::Modulo20, 3, Couple { aggressor: 21, victim: 20, mask: !0 })]
#[case::modulo_alias(TestKind::Modulo20, 1, Alias { from: 21, to: 20 })]
#[case::block_move_stuck(TestKind::BlockMove, 4, Flip { word: 77, mask: 1 << 5 })]
#[case::block_move_coupling(TestKind::BlockMove, 4, Couple { aggressor: 600, victim: 1601, mask: !0 })]
#[case::bus_stress_low_half(TestKind::BusStress, 0, Flip { word: 500, mask: 1 << 20 })]
#[case::bus_stress_high_half(TestKind::BusStress, 1, Flip { word: 1500, mask: 1 << 21 })]
#[case::bit_fade_ones(TestKind::BitFade, 4, Flip { word: 1234, mask: 1 << 17 })]
#[case::bit_fade_random(TestKind::BitFade, 7, Flip { word: 4, mask: 1 << 61 })]
fn an_injected_fault_is_reported_exactly_on_every_tier(
    #[case] kind: TestKind,
    #[case] after_phase: usize,
    #[case] injection: Injection,
) {
    for layout in LAYOUTS {
        for kernel in kernels() {
            let (faults, want) =
                run(kind, Kernels::uniform(kernel), &mut Mem::new(layout), Some((after_phase, injection)));
            assert!(let Some(want) = want, "{kind:?}: phase {after_phase} never ended");
            check!(faults == [want], "{kind:?} on {kernel} over {layout:?}");
        }
    }
}

/// A decoder with address bit `bit` stuck at 0: an address with the bit set reaches the cell without it.
struct StuckAddressLine {
    bit: usize,
}

impl StuckAddressLine {
    fn cell(&self, p: *mut u64) -> *mut u64 {
        p.map_addr(|a| a & !(1 << self.bit))
    }
}

impl Cells for StuckAddressLine {
    unsafe fn write(&mut self, p: *mut u64, v: u64) {
        // SAFETY: the cell is the anchor or an offset of it inside the span.
        unsafe { self.cell(p).write(v) };
    }

    unsafe fn read(&mut self, p: *mut u64) -> u64 {
        // SAFETY: as `write`.
        unsafe { self.cell(p).read() }
    }
}

#[rstest]
#[case::low_bit(4)]
#[case::high_bit(13)]
fn the_address_walk_catches_a_stuck_address_line(#[case] bit: usize) {
    let span = 1 << 14;
    let mut buf = Buf::new(2 * span / 8);
    let region = buf.region();
    let base = region.addr(0).get();
    let offset = (base.next_multiple_of(span) - base) / 8;
    let (anchor, anchor_ptr) = (region.addr(offset), region.ptr().wrapping_add(offset));
    let mut reported = Vec::new();
    let mut sink = |f| reported.push(f);
    let mut faults = Faults::new(4, &mut sink);
    // SAFETY: the span lies inside the region.
    unsafe { address_walk(&mut StuckAddressLine { bit }, anchor_ptr, span, 0, &mut faults) };
    let partner = anchor.get() + (1 << bit);
    let want = Fault { addr: anchor, expected: anchor.get() as u64, actual: !(partner as u64) };
    assert!(let Some(first) = reported.first());
    check!(*first == want);
}

#[test]
fn descending_prsg_and_rotating_inversions_undo_the_ascending_ones() {
    const SEED: u64 = 0x1234_5678;
    const PATTERN: u64 = 1 << 5;
    let chunk = 5 * BLOCK_WORDS;
    let blocks = (WORDS / BLOCK_WORDS) as u64;
    for kernel in kernels() {
        let mut buf = Buf::new(WORDS);
        let mut region = buf.region();
        let mut reported = Vec::new();
        let mut sink = |f| reported.push(f);
        let mut faults = Faults::new(8, &mut sink);
        let up = Inversion { expect: 0, write: !0, order: Order::Ascending };
        let down = Inversion { expect: !0, write: 0, order: Order::Descending };

        fill_prsg(kernel, &mut region, &mut Prsg::new(SEED), 0);
        let mut prsg = Prsg::new(SEED);
        region.chunks(chunk).for_each(|mut c| invert_prsg(kernel, &mut c, &mut prsg, up, &mut faults));
        check!(prsg == Prsg::at(SEED, blocks), "{kernel}");
        check!((0..WORDS).all(|w| region.peek(w) == !Prsg::word(SEED, w as u64)), "{kernel}");
        region.chunks(chunk).rev().for_each(|mut c| invert_prsg(kernel, &mut c, &mut prsg, down, &mut faults));
        check!(prsg == Prsg::new(SEED), "{kernel}");
        check!((0..WORDS).all(|w| region.peek(w) == Prsg::word(SEED, w as u64)), "{kernel}");

        fill_rotating(kernel, &mut region, &mut Rotating::at(PATTERN, 0), 0);
        let mut rot = Rotating::at(PATTERN, 0);
        region.chunks(chunk).for_each(|mut c| invert_rotating(kernel, &mut c, &mut rot, up, &mut faults));
        check!((0..WORDS).all(|w| region.peek(w) == !Rotating::word(PATTERN, w as u64)), "{kernel}");
        region.chunks(chunk).rev().for_each(|mut c| invert_rotating(kernel, &mut c, &mut rot, down, &mut faults));
        check!(rot == Rotating::at(PATTERN, 0), "{kernel}");
        check!((0..WORDS).all(|w| region.peek(w) == Rotating::word(PATTERN, w as u64)), "{kernel}");
        check!(reported.is_empty(), "{kernel}");
    }
}

#[test]
fn a_descending_prsg_inversion_reports_faults_top_down() {
    const SEED: u64 = 99;
    let (low, high) = (3, WORDS - 10);
    for kernel in kernels() {
        let mut buf = Buf::new(WORDS);
        let mut region = buf.region();
        fill_prsg(kernel, &mut region, &mut Prsg::new(SEED), !0);
        region.poke(low, region.peek(low) ^ 1);
        region.poke(high, region.peek(high) ^ 1 << 40);
        let mut reported = Vec::new();
        let mut sink = |f: Fault| reported.push((f.addr, f.expected));
        let mut faults = Faults::new(8, &mut sink);
        let mut prsg = Prsg::at(SEED, (WORDS / BLOCK_WORDS) as u64);
        let step = Inversion { expect: !0, write: 0, order: Order::Descending };
        region
            .chunks(3 * BLOCK_WORDS)
            .rev()
            .for_each(|mut c| invert_prsg(kernel, &mut c, &mut prsg, step, &mut faults));
        let expected = |w: usize| !Prsg::word(SEED, w as u64);
        check!(reported == [(region.addr(high), expected(high)), (region.addr(low), expected(low))], "{kernel}");
    }
}

#[test]
fn a_suite_runs_its_preset_in_order_pass_after_pass() {
    let mut mem = Mem::new(RAGGED);
    let mut sink = |_| {};
    let mut faults = Faults::new(0, &mut sink);
    let mut suite = Suite::new(mem.regions(), Kernels::uniform(Kernel::SCALAR), CFG, Preset::Quick);
    let mut seen = Vec::new();
    let mut last_percent = 0;
    while suite.passes() < 2 {
        let entry = (suite.test().kind(), suite.position(), suite.passes());
        if seen.last() != Some(&entry) {
            seen.push(entry);
        }
        let percent = suite.percent();
        check!(percent >= last_percent || entry.1.0 == 1);
        last_percent = percent;
        check!(suite.step(64 << 10, Instant::default(), &mut faults) != Progress::Done);
    }
    let one_pass: Vec<_> = TestKind::sequence(Preset::Quick).collect();
    let n = one_pass.len();
    let want: Vec<_> =
        (0..2).flat_map(|pass| one_pass.iter().enumerate().map(move |(i, &k)| (k, (i + 1, n), pass))).collect();
    check!(seen == want);
    check!(faults.seen() == 0);
}

#[rstest]
#[case::two(2)]
#[case::three(3)]
#[case::five(5)]
fn shares_tile_the_set_and_keep_move_windows_whole(#[case] parts: usize) {
    let parts = NonZeroUsize::new(parts).unwrap();
    for layout in ALL_LAYOUTS {
        for &kind in TestKind::ALL {
            let mut mem = Mem::new(layout);
            let mut regions = mem.regions();
            let addrs: Vec<usize> = (0..WORDS).map(|w| regions.addr(w).get()).collect();
            let whole = regions.layout();
            let got: Vec<(Span, usize, usize)> =
                shares(&mut regions, kind, &CFG, parts).map(|(span, r)| (span, r.addr(0).get(), r.words())).collect();
            let mut next = 0;
            for &(span, first, words) in &got {
                check!(span.start == next, "{kind:?} over {layout:?}");
                check!(first == addrs[span.start], "{kind:?} over {layout:?}");
                check!(words == span.words && words > 0 && words % BLOCK_WORDS == 0, "{kind:?} over {layout:?}");
                next = span.end();
            }
            check!(next == WORDS, "{kind:?} over {layout:?}");
            let want_one = kind.mode() == Mode::Single;
            check!((got.len() == 1) == want_one, "{kind:?}: {} shares", got.len());
            for window in plan::move_windows(&whole, &CFG) {
                check!(got.iter().filter(|(s, ..)| s.contains(window)).count() == 1, "{window:?} spans two shares");
                check!(whole.spans().iter().any(|r| r.contains(window)), "{window:?} spans two regions");
            }
        }
    }
}

#[rstest]
#[case::two(2)]
#[case::three(3)]
#[case::five(5)]
fn shares_are_balanced_to_a_window(#[case] parts: usize) {
    let layout = Layout::new([4096, 2048 + 128, 640]);
    let window = plan::move_window(&CFG);
    let cuts = share::share_spans(&layout, TestKind::SolidBits, &CFG, NonZeroUsize::new(parts).unwrap());
    let sizes: Vec<usize> = cuts.iter().map(|s| s.words).collect();
    check!(cuts.len() == parts, "{sizes:?}");
    check!(sizes.iter().sum::<usize>() == layout.words(), "{sizes:?}");
    let (big, small) = (sizes.iter().max().unwrap(), sizes.iter().min().unwrap());
    check!(big - small <= 2 * window, "{sizes:?}");
}

/// Words of the regions behind `shares`, read through them in order.
fn snapshot(shares: &[(Span, Regions<'_>)]) -> Vec<u64> {
    shares.iter().flat_map(|(_, r)| (0..r.words()).map(|w| r.peek(w))).collect()
}

/// [`Injection::apply`] on the regions behind `shares`, by word of the whole set.
fn inject_split(injection: Injection, shares: &mut [(Span, Regions<'_>)]) {
    let find = |w: usize| shares.iter().position(|(s, _)| s.contains(Span { start: w, words: 1 })).unwrap();
    let peek = |w: usize| {
        let (span, region) = &shares[find(w)];
        region.peek(w - span.start)
    };
    let (victim, actual) = match injection {
        Flip { word, mask } => (word, peek(word) ^ mask),
        Couple { aggressor, victim, mask } => (victim, peek(victim) ^ (peek(aggressor) & mask)),
        Alias { from, to } => (to, peek(from)),
    };
    let (span, region) = &mut shares[find(victim)];
    region.poke(victim - span.start, actual);
}

/// A fault by its word index in the set, as single and split runs over different buffers can
/// only be compared.
type At = (usize, u64, u64);

/// The word of the set at `addr`, given each word's address.
fn word_at(addrs: &[usize], f: &Fault) -> usize {
    addrs.iter().position(|&a| a == f.addr.get()).unwrap()
}

/// Run `kind` over the whole set of regions laid out as `layout` and split `parts` ways, phase by
/// phase in lockstep, injecting after phase `after` in both. Memory must match after every phase,
/// and the faults must too; the split run's faults must come from the share holding the word. Both
/// runs use the same memory, one after the other.
fn split_matches_whole(kind: TestKind, layout: &[usize], parts: usize, inject: Option<(usize, Injection)>) {
    // SAFETY: the flags come from the host's own feature detection.
    let caps = unsafe { Caps::new(std::is_x86_feature_detected!("avx2"), std::is_x86_feature_detected!("avx512f")) };
    let kernels = Kernels::for_caps(caps);
    let mut now = Instant::default();

    let mut mem = Mem::new(layout);
    let mut whole_region = mem.regions();
    let addrs: Vec<usize> = (0..WORDS).map(|w| whole_region.addr(w).get()).collect();
    let mut whole = Test::new(kind, CFG, 0, &whole_region);
    let mut whole_faults: Vec<At> = Vec::new();
    let mut whole_memory = Vec::new();
    for p in 0..whole.phases() {
        while whole.phase() == p {
            let mut sink = |f: Fault| whole_faults.push((word_at(&addrs, &f), f.expected, f.actual));
            let mut faults = Faults::new(64, &mut sink);
            if let Progress::Waiting { until } = whole.step(&mut whole_region, &kernels, CHUNK_BYTES, now, &mut faults)
            {
                now = until;
            }
        }
        if let Some((_, injection)) = inject.filter(|&(after, _)| after == p) {
            injection.apply(&mut whole_region);
        }
        whole_memory.push((0..WORDS).map(|w| whole_region.peek(w)).collect::<Vec<_>>());
    }

    // The same memory, cleared: the address tests write the addresses themselves.
    let mut split_region = mem.regions();
    (0..WORDS).for_each(|w| split_region.poke(w, 0));
    let addrs: Vec<usize> = (0..WORDS).map(|w| split_region.addr(w).get()).collect();
    let whole_layout = split_region.layout();
    let parts = NonZeroUsize::new(parts).unwrap();
    let mut split: Vec<(Span, Regions<'_>)> = shares(&mut split_region, kind, &CFG, parts).collect();
    let mut tests: Vec<Test> =
        split.iter().map(|&(span, _)| Test::for_share(kind, CFG, 0, &whole_layout, span)).collect();
    check!(tests.iter().map(Test::total).sum::<u64>() == whole.total(), "{kind:?}: the shares' work adds up");
    let mut split_faults: Vec<(usize, At)> = Vec::new();
    for (p, want) in whole_memory.iter().enumerate() {
        for (core, (test, (_, region))) in tests.iter_mut().zip(&mut split).enumerate() {
            check!(test.phases() == whole.phases());
            while test.phase() == p {
                let mut sink = |f: Fault| split_faults.push((core, (word_at(&addrs, &f), f.expected, f.actual)));
                let mut faults = Faults::new(64, &mut sink);
                if let Progress::Waiting { until } = test.step(region, &kernels, CHUNK_BYTES, now, &mut faults) {
                    now = until;
                }
            }
        }
        if let Some((_, injection)) = inject.filter(|&(after, _)| after == p) {
            inject_split(injection, &mut split);
        }
        check!(snapshot(&split) == *want, "{kind:?} split {parts} ways differs after phase {p}");
    }
    check!(tests.iter().all(|t| t.done() == t.total()), "{kind:?}");
    for &(core, (word, ..)) in &split_faults {
        check!(split[core].0.contains(Span { start: word, words: 1 }), "{kind:?}: word {word} found by core {core}");
    }
    let mut split_faults: Vec<At> = split_faults.into_iter().map(|(_, at)| at).collect();
    split_faults.sort_unstable();
    whole_faults.sort_unstable();
    check!(split_faults == whole_faults, "{kind:?}");
    if inject.is_some() {
        check!(!whole_faults.is_empty(), "{kind:?}: the injection was caught");
    }
}

#[rstest]
#[case::two(2)]
#[case::three(3)]
#[case::five(5)]
fn every_split_test_does_what_the_whole_set_run_does(#[case] parts: usize) {
    for layout in ALL_LAYOUTS {
        for &kind in TestKind::ALL.iter().filter(|k| k.mode() == Mode::Split) {
            split_matches_whole(kind, layout, parts, None);
        }
    }
}

#[rstest]
#[case::modulo_stride_in_a_later_share(TestKind::Modulo20, 0, Flip { word: 700, mask: 1 << 33 })]
#[case::modulo_coupling_at_a_share_edge(TestKind::Modulo20, 3, Couple { aggressor: 1024, victim: 1020, mask: !0 })]
#[case::random_inversions_past_the_first_share(TestKind::MovingInversionsRandom, 1, Flip { word: 1400, mask: !0 })]
#[case::shifting_inversions_descending(TestKind::MovingInversions64Bit, 2, Flip { word: 1999, mask: 1 << 3 })]
#[case::own_address_alias_across_shares(TestKind::OwnAddress, 0, Alias { from: 1000, to: 300 })]
#[case::own_address_complement_alias_across_shares(TestKind::OwnAddress, 2, Alias { from: 8, to: 2040 })]
#[case::own_address_window_alias(TestKind::OwnAddressWindow, 2, Alias { from: 512, to: 600 })]
#[case::march_stuck_in_a_later_share(TestKind::MarchCMinus, 0, Flip { word: 1513, mask: 1 << 3 })]
#[case::march_transition_in_a_descending_element(TestKind::MarchCMinus, 2, Flip { word: 64, mask: !0 })]
#[case::march_coupling_at_a_share_edge(TestKind::MarchCMinus, 3, Couple { aggressor: 1024, victim: 1020, mask: 1 << 5 })]
#[case::random_inversions_alias_in_the_merged_pass(TestKind::MovingInversionsRandom, 2, Alias { from: 100, to: 1900 })]
#[case::bus_stress_high_half(TestKind::BusStress, 1, Flip { word: 1500, mask: 1 << 21 })]
#[case::bus_stress_last_verify(TestKind::BusStress, 5, Flip { word: 2000, mask: 1 })]
#[case::block_move_window(TestKind::BlockMove, 4, Couple { aggressor: 600, victim: 1601, mask: !0 })]
#[case::bit_fade_random(TestKind::BitFade, 7, Flip { word: 1100, mask: 1 << 61 })]
fn an_injected_fault_is_found_split_as_whole(
    #[case] kind: TestKind,
    #[case] after: usize,
    #[case] injection: Injection,
) {
    for layout in LAYOUTS {
        for parts in [3, 4] {
            split_matches_whole(kind, layout, parts, Some((after, injection)));
        }
    }
}

/// Every phase of `kind` over `layout` as planned with `cfg`.
fn phases(kind: TestKind, cfg: &Config, layout: &Layout) -> Vec<plan::Phase> {
    (0..).map_while(|i| plan::nth(kind, cfg, 0, layout, i)).collect()
}

#[rstest]
#[case::ones_and_zeros(TestKind::MovingInversions)]
#[case::bytes(TestKind::MovingInversions8Bit)]
#[case::random(TestKind::MovingInversionsRandom)]
#[case::shifting(TestKind::MovingInversions64Bit)]
fn a_moving_inversions_round_ends_in_the_next_rounds_fill(#[case] kind: TestKind) {
    let cfg = Config { fixed_iterations: 2, byte_iterations: 2, shifting_iterations: 2, ..CFG };
    let layout = Layout::new([WORDS]);
    let ops: Vec<plan::Op> = phases(kind, &cfg, &layout).into_iter().map(|p| p.op).collect();
    let fills =
        |op: &plan::Op| matches!(op, plan::Op::Fill(_) | plan::Op::FillPrsg { .. } | plan::Op::FillRotating { .. });
    check!(ops.iter().filter(|op| fills(op)).count() == 1, "{kind:?}: only the first round fills");
    check!(fills(&ops[0]));
    // A pattern followed by its complement leaves the words as they are, so no pass is left.
    let merged = ops.iter().any(|op| matches!(op, plan::Op::Replace { .. }));
    check!(merged != (kind == TestKind::MovingInversions), "{kind:?}");
    let restores = |op: &&plan::Op| match op {
        plan::Op::Invert(step) | plan::Op::InvertPrsg { step, .. } | plan::Op::InvertRotating { step, .. } => {
            step.order == Order::Descending
        },
        _ => false,
    };
    let last = ops.last().unwrap();
    check!(restores(&last), "{kind:?}: the last round still restores its pattern");
}

#[test]
fn modulo_fills_the_rest_once_per_stride_offset_and_value() {
    let layout = Layout::new([WORDS]);
    let ops: Vec<plan::Op> = phases(TestKind::Modulo20, &CFG, &layout).into_iter().map(|p| p.op).collect();
    let except = ops.iter().filter(|op| matches!(op, plan::Op::FillExcept { .. })).count();
    check!(except == 2 * plan::MODULO * CFG.modulo_rounds as usize);
    check!(ops.windows(2).all(|w| !matches!(w, [plan::Op::FillExcept { .. }, plan::Op::FillExcept { .. }])));
}

#[test]
fn the_address_walk_runs_in_every_region() {
    const UNTOUCHED: u64 = 0x5eed_5eed_5eed_5eed;
    let mut mem = Mem::new(RAGGED);
    let mut regions = mem.regions();
    let layout = regions.layout();
    (0..WORDS).for_each(|w| regions.poke(w, UNTOUCHED));
    let walks: Vec<Span> = (0..)
        .map_while(|i| plan::nth(TestKind::AddressWalk, &CFG, 0, &layout, i))
        .filter(|p| matches!(p.op, plan::Op::AddressWalk { .. }))
        .map(|p| p.span)
        .collect();
    check!(walks == [layout.spans(), layout.spans()].concat(), "each region once per polarity");

    let mut sink = |_| {};
    let mut faults = Faults::new(0, &mut sink);
    let mut test = Test::new(TestKind::AddressWalk, CFG, 0, &regions);
    while test.step(&mut regions, &Kernels::uniform(Kernel::SCALAR), CHUNK_BYTES, Instant::default(), &mut faults)
        != Progress::Done
    {}
    for span in layout.spans() {
        check!((span.start..span.end()).any(|w| regions.peek(w) != UNTOUCHED), "{span:?} was walked");
    }
}

#[test]
fn move_windows_stay_inside_their_region() {
    let layout = Layout::new(RAGGED.iter().copied());
    let windows: Vec<Span> = plan::move_windows(&layout, &CFG).collect();
    check!(windows.iter().map(|w| w.words).sum::<usize>() == WORDS, "every word is in a window");
    for window in &windows {
        check!(layout.spans().iter().filter(|r| r.contains(*window)).count() == 1, "{window:?}");
    }
    check!(windows.iter().any(|w| w.words < plan::move_window(&CFG)), "a region's last window is short");
}

#[test]
fn the_memtest86_plus_preset_runs_memtest86_plus_tests_with_its_counts() {
    let preset = Preset::Memtest86Plus;
    check!(<&str>::from(preset) == "memtest86-plus");
    let kinds: Vec<TestKind> = TestKind::sequence(preset).collect();
    check!(kinds.len() == 11);
    check!(!kinds.iter().any(|k| matches!(k, TestKind::SolidBits | TestKind::MarchCMinus | TestKind::WalkingOnes)));
    check!(kinds.contains(&TestKind::BitFade) && kinds.contains(&TestKind::AddressWalk));
    let config = preset.config();
    check!((config.random_rounds, config.modulo_rounds, config.fade) == (128, 8, Duration::from_secs(120)));
    check!((config.fixed_iterations, config.byte_iterations, config.move_iterations) == (6, 3, 81));
    check!(Preset::Thorough.config() == Config::DEFAULT);
    check!(TestKind::sequence(Preset::Thorough).count() == TestKind::ALL.len());
}
