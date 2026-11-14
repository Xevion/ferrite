use alloc::vec;
use alloc::vec::Vec;
use core::time::Duration;

use assert2::{assert, check};
use ferrite_hal::Instant;
use rstest::rstest;

use super::*;
use crate::region::Layout;
use crate::suite::{Preset, Stage, TestKind};

fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

fn at(s: u64) -> Instant {
    Instant::from_epoch(secs(s))
}

/// Three plain tests of 1000 words.
fn plain(n: usize) -> Vec<Shape> {
    vec![Shape::new([Stage::Work(1000)]); n]
}

fn estimator(priors: [Option<u64>; 3]) -> Estimator {
    Estimator::new(plain(3), priors.map(|p| p.map(secs)).to_vec())
}

#[test]
fn with_priors_every_test_counts_for_what_it_took_before() {
    let mut est = estimator([Some(10), Some(20), Some(30)]);
    let f = est.observe(at(0), 0, 0, None);
    assert!(let Some(eta) = f.shown);
    check!(eta == Eta { remaining: secs(60), confidence: Confidence::Measured });
    check!(f.pass_total == Some(secs(60)));
    // Half the first test done on time: nothing moves but the clock.
    let f = est.observe(at(5), 0, 500, None);
    check!(f.shown.map(|e| e.remaining) == Some(secs(55)));
}

#[test]
fn the_running_test_trusts_its_own_line_more_the_further_it_is() {
    // A prior of 100 s; the test is in fact going to take 200 s.
    let remaining_at = |elapsed: u64, done: u64| {
        let mut est = estimator([Some(100), Some(100), Some(100)]);
        est.observe(at(0), 0, 0, None);
        est.observe(at(elapsed), 0, done, None).raw.map(|e| e.remaining)
    };
    // At 10% (20 s in) the blend is 40% of the way from the prior (80 s left) to the line (180 s).
    check!(remaining_at(20, 100) == Some(secs(80 + 40) + secs(200)));
    // At a quarter (50 s in) it is all line: 150 s left.
    check!(remaining_at(50, 250) == Some(secs(150) + secs(200)));
}

#[rstest]
#[case::on_prior(50, 500, Confidence::Measured)]
#[case::within_a_tenth(54, 500, Confidence::Measured)]
#[case::too_slow(60, 500, Confidence::Estimated)]
#[case::too_fast(40, 500, Confidence::Estimated)]
#[case::too_early_to_tell(20, 100, Confidence::Measured)]
fn a_test_running_at_another_speed_than_its_prior_is_only_estimated(
    #[case] elapsed: u64,
    #[case] done: u64,
    #[case] confidence: Confidence,
) {
    let mut est = estimator([Some(100), Some(100), Some(100)]);
    est.observe(at(0), 0, 0, None);
    let f = est.observe(at(elapsed), 0, done, None);
    check!(f.raw.map(|e| e.confidence) == Some(confidence));
}

#[test]
fn a_test_without_a_prior_makes_the_whole_prediction_an_estimate() {
    let mut est = estimator([Some(100), None, Some(100)]);
    est.observe(at(0), 0, 0, None);
    let f = est.observe(at(10), 0, 100, None);
    check!(f.raw.map(|e| e.confidence) == Some(Confidence::Estimated));
}

#[test]
fn nothing_is_predicted_before_there_is_anything_to_go_on() {
    let mut est = estimator([None, None, None]);
    let f = est.observe(at(0), 0, 0, None);
    check!(f.raw.is_none() && f.shown.is_none() && f.test_left.is_none());
    // Once the first test has run a while, the rest are its words at its speed.
    let f = est.observe(at(10), 0, 500, None);
    check!(f.raw.map(|e| e.remaining) == Some(secs(10 + 40)));
    check!(f.shown.is_none(), "one sample says nothing about agreement");
    est.finish(0, at(20), secs(20));
    // A finished test sets the pace for the others: 20 s per 1000 words.
    let f = est.observe(at(20), 1, 0, None);
    check!(f.raw.map(|e| e.remaining) == Some(secs(40)), "the running test's guess is the pace too");
}

#[test]
fn an_estimate_is_shown_only_once_a_few_seconds_of_predictions_agree() {
    let mut est = estimator([None, None, None]);
    est.observe(at(0), 0, 0, None);
    let mut shown = Vec::new();
    for s in 1..=12 {
        // Steady speed: 50 words a second.
        shown.push(est.observe(at(s), 0, s * 50, None).shown.is_some());
    }
    check!(shown[..5] == [false; 5], "less than five seconds of agreement: {shown:?}");
    check!(shown[5..].iter().all(|&s| s), "then shown, once the first sample is five seconds old: {shown:?}");
    // The test turns out to be much slower than it looked: the predictions disagree again.
    let f = est.observe(at(13), 0, 12 * 50 + 1, None);
    check!(f.raw.is_some());
    check!(f.shown.is_none());
}

#[test]
fn a_wait_is_exact_and_work_stands_still_through_it() {
    let shape = Shape::new([Stage::Work(100), Stage::Wait(secs(120)), Stage::Work(100)]);
    let mut est = Estimator::new(vec![shape], vec![Some(secs(130))]);
    est.observe(at(0), 0, 0, None);
    let f = est.observe(at(5), 0, 100, None);
    check!(f.position == Position::Wait { nth: 1, waits: 1, left: secs(120) });
    let f = est.observe(at(65), 0, 100, None);
    check!(f.position == Position::Wait { nth: 1, waits: 1, left: secs(60) });
    // 60 s of the wait are gone; the work after it is expected to take what the prior leaves.
    check!(f.test_left == Some(secs(60) + secs(5)));
    let f = est.observe(at(125), 0, 100, None);
    check!(f.position == Position::Wait { nth: 1, waits: 1, left: Duration::ZERO });
    // 7 s of work for 140 of 200 words: the line says 10 s in all.
    let f = est.observe(at(127), 0, 140, None);
    check!(f.position == Position::Work { waits_passed: 1, waits: 1 });
    check!(f.test_left == Some(secs(3)), "{:?}", f.test_left);
}

#[test]
fn a_caller_that_knows_when_the_wait_ends_places_its_start_exactly() {
    let shape = Shape::new([Stage::Work(100), Stage::Wait(secs(120)), Stage::Work(100)]);
    let mut est = Estimator::new(vec![shape], vec![Some(secs(130))]);
    est.observe(at(0), 0, 0, None);
    // First seen 7 s late; the firmware says it ends at 125, so it began at 5.
    let f = est.observe(at(12), 0, 100, Some(at(125)));
    check!(f.position == Position::Wait { nth: 1, waits: 1, left: secs(113) });
}

#[test]
fn time_the_run_was_paused_does_not_count_against_the_test() {
    let mut est = estimator([None, None, None]);
    est.observe(at(0), 0, 0, None);
    est.observe(at(10), 0, 100, None);
    // Paused for a minute, then carrying on.
    est.shift(secs(60));
    let f = est.observe(at(80), 0, 200, None);
    check!(f.raw.map(|e| e.remaining) == Some(secs(80) + secs(200)), "{:?}", f.raw);
}

#[test]
fn a_test_that_starts_over_is_timed_from_the_restart() {
    let mut est = estimator([None, None, None]);
    est.observe(at(0), 0, 0, None);
    est.observe(at(50), 0, 500, None);
    // A split test abandoned for a command runs again from nothing.
    est.observe(at(60), 0, 0, None);
    let f = est.observe(at(70), 0, 100, None);
    check!(f.raw.map(|e| e.remaining) == Some(secs(90) + secs(200)), "{:?}", f.raw);
}

#[test]
fn finished_times_become_the_next_passes_priors() {
    let mut est = estimator([None, None, None]);
    let mut now = 0;
    for (i, took) in [10, 20, 30].into_iter().enumerate() {
        est.observe(at(now), i, 0, None);
        now += took;
        est.finish(i, at(now), secs(took));
    }
    est.begin_pass();
    check!(est.known() == 3);
    let f = est.observe(at(now), 0, 0, None);
    check!(f.shown == Some(Eta { remaining: secs(60), confidence: Confidence::Measured }));
}

mod replay {
    use super::*;

    /// One test of a recorded pass: what it took and a few `(ms into the test, percent)` samples.
    struct Row {
        name: &'static str,
        took_ms: u64,
        samples: &'static [(u64, u64)],
    }

    /// A full pass of the memtest86+ preset over 15.9 GB on 8 logical processors (the X1 Carbon), from
    /// the sweep's own log: each test's time and a few of its 30-second progress samples. Tests
    /// too short to have been sampled have none. Bit fade has four waits of 120 s, which are in its
    /// time; its progress between them is not sampled.
    const PASS: [Row; 11] = [
        Row { name: "address, walking ones", took_ms: 4, samples: &[] },
        Row { name: "address, own address", took_ms: 2_755, samples: &[] },
        Row { name: "bus stress, r/w turnaround", took_ms: 70_809, samples: &[(16_861, 23), (46_861, 66)] },
        Row { name: "moving inversions, 1s and 0s", took_ms: 30_116, samples: &[(6_052, 21)] },
        Row {
            name: "moving inversions, random",
            took_ms: 333_048,
            samples: &[(25_394, 7), (115_394, 34), (205_394, 61), (325_394, 97)],
        },
        Row {
            name: "moving inversions, 8-bit",
            took_ms: 113_506,
            samples: &[(22_340, 20), (52_340, 46), (82_340, 72), (112_341, 98)],
        },
        Row {
            name: "modulo 20, random",
            took_ms: 562_523,
            samples: &[(28_842, 5), (148_841, 26), (268_841, 47), (388_841, 69), (508_842, 90)],
        },
        Row { name: "block move", took_ms: 110_009, samples: &[(6_311, 5), (36_311, 33), (66_312, 60), (96_312, 88)] },
        Row {
            name: "moving inversions, 64-bit shifting",
            took_ms: 246_790,
            samples: &[(16_305, 6), (76_305, 31), (136_305, 55), (196_304, 79), (226_305, 91)],
        },
        Row { name: "address, own address in window", took_ms: 1_816, samples: &[] },
        Row { name: "bit fade", took_ms: 485_968, samples: &[] },
    ];

    /// What the same tests took on the next pass, where that was logged (the others repeat).
    const NEXT_PASS_MS: [Option<u64>; 11] = [
        Some(6),
        Some(2_854),
        Some(70_879),
        Some(30_125),
        Some(332_984),
        Some(113_506),
        Some(562_403),
        None,
        None,
        None,
        None,
    ];

    const BYTES: usize = 15_942_901_760;

    fn shapes() -> Vec<Shape> {
        let layout = Layout::new([BYTES / 8]);
        let cfg = Preset::Memtest86Plus.config();
        TestKind::sequence(Preset::Memtest86Plus).map(|k| Shape::of(k, &cfg, 0, &layout)).collect()
    }

    /// Words done `off_ms` into test `row`, from its samples (a straight line between them).
    fn done_at(shape: &Shape, row: &Row, off_ms: u64) -> u64 {
        let work = shape.work();
        if shape.wait_count() > 0 {
            // Work stages share the time that is not waiting, in proportion to their words.
            let work_ms = row.took_ms - u64::try_from(shape.waits().as_millis()).unwrap();
            let (mut t, mut words) = (0, 0);
            for stage in shape.stages() {
                match *stage {
                    Stage::Work(w) => {
                        let ms = work_ms * w / work;
                        if off_ms < t + ms {
                            return words + w * (off_ms - t) / ms;
                        }
                        t += ms;
                        words += w;
                    },
                    Stage::Wait(d) => {
                        let ms = u64::try_from(d.as_millis()).unwrap();
                        if off_ms < t + ms {
                            return words;
                        }
                        t += ms;
                    },
                }
            }
            return work;
        }
        let knots: Vec<(u64, u64)> = core::iter::once((0, 0))
            .chain(row.samples.iter().map(|&(ms, pct)| (ms, pct * 10)))
            .chain([(row.took_ms, 1000)])
            .collect();
        let pair = knots.windows(2).find(|w| off_ms < w[1].0).unwrap_or_else(|| &knots[knots.len() - 2..]);
        let (t0, p0, t1, p1) = (pair[0].0, pair[0].1, pair[1].0, pair[1].1);
        let permille = p0 + (p1 - p0) * (off_ms.min(t1) - t0) / (t1 - t0);
        work * permille / 1000
    }

    /// One observation of the replay.
    struct Tick {
        at_ms: u64,
        test: usize,
        forecast: Forecast,
        /// The pass's true length and the time truly left in it.
        pass_ms: u64,
        left_ms: u64,
    }

    impl Tick {
        /// Prediction error in ms, positive when too long.
        fn error_ms(eta: &Eta, left_ms: u64) -> i64 {
            i64::try_from(eta.remaining.as_millis()).unwrap() - i64::try_from(left_ms).unwrap()
        }
    }

    /// The pass played back second by second through an estimator with `priors`.
    fn play(priors: Vec<Option<Duration>>) -> Vec<Tick> {
        let shapes = shapes();
        let mut est = Estimator::new(shapes.clone(), priors);
        let starts: Vec<u64> = PASS
            .iter()
            .scan(0, |t, r| {
                let start = *t;
                *t += r.took_ms;
                Some(start)
            })
            .collect();
        let pass_ms: u64 = PASS.iter().map(|r| r.took_ms).sum();
        let now = |ms: u64| Instant::from_epoch(Duration::from_millis(ms));
        let mut ticks = Vec::new();
        for (i, row) in PASS.iter().enumerate() {
            let (start, end) = (starts[i], starts[i] + row.took_ms);
            let moments = std::iter::once(start).chain((start / 1000 + 1) * 1000..end).step_by(1);
            for t in moments.filter(|t| *t == start || t % 1000 == 0) {
                let done = done_at(&shapes[i], row, t - start);
                let forecast = est.observe(now(t), i, done, None);
                ticks.push(Tick { at_ms: t, test: i, forecast, pass_ms, left_ms: pass_ms - t });
            }
            est.finish(i, now(end), Duration::from_millis(row.took_ms));
        }
        ticks
    }

    fn priors() -> Vec<Option<Duration>> {
        PASS.iter()
            .zip(NEXT_PASS_MS)
            .map(|(row, next)| Some(Duration::from_millis(next.unwrap_or(row.took_ms))))
            .collect()
    }

    #[test]
    fn the_fixture_is_the_memtest86_plus_sequence() {
        let names: Vec<&str> = TestKind::sequence(Preset::Memtest86Plus).map(TestKind::name).collect();
        check!(PASS.iter().map(|r| r.name).collect::<Vec<_>>() == names);
    }

    /// Error of the ETA shown at `tick`, as a fraction of `of`; `None` when nothing is shown.
    #[expect(clippy::cast_precision_loss, reason = "milliseconds of a pass are far below 2^52")]
    fn shown_error(tick: &Tick, of: u64) -> Option<f64> {
        let eta = tick.forecast.shown?;
        Some(Tick::error_ms(&eta, tick.left_ms).unsigned_abs() as f64 / of as f64)
    }

    /// The worst of `shown_error` over the ticks `from_ms..to_ms` into the pass, each against the time
    /// left then (but never less than ten seconds, where a fraction means nothing).
    fn worst_of_left(ticks: &[Tick], from_ms: u64, to_ms: u64) -> f64 {
        let in_range = ticks.iter().filter(|t| (from_ms..to_ms).contains(&t.at_ms));
        in_range.filter_map(|t| shown_error(t, t.left_ms.max(10_000))).fold(0.0, f64::max)
    }

    #[test]
    fn with_priors_the_end_is_shown_from_the_first_second_and_is_right_to_seconds() {
        let ticks = play(priors());
        check!(ticks.iter().all(|t| t.forecast.shown.is_some()));
        check!(ticks[0].forecast.shown.map(|e| e.confidence) == Some(Confidence::Measured));
        // Measured over this fixture: 0.5% of the time left, 4.6% in the last ten seconds.
        let worst = worst_of_left(&ticks, 60_000, u64::MAX);
        check!(worst < 0.05, "worst error {worst}");
        let seconds = ticks.iter().filter_map(|t| t.forecast.shown.map(|e| Tick::error_ms(&e, t.left_ms).abs())).max();
        check!(
            seconds.unwrap_or(0) < 15_000,
            "worst error {seconds:?} ms of a {} s pass",
            PASS.iter().map(|r| r.took_ms).sum::<u64>() / 1000
        );
    }

    #[test]
    fn without_priors_a_first_pass_is_hidden_then_estimated_then_close() {
        let ticks = play(vec![None; 11]);
        let first = ticks.iter().find(|t| t.forecast.shown.is_some()).expect("shown at some point");
        check!(first.at_ms >= 5_000, "agreement needs five seconds of predictions");
        check!(first.at_ms <= 60_000, "first shown at {} s", first.at_ms / 1000);
        check!(ticks.iter().filter_map(|t| t.forecast.shown).all(|e| e.confidence == Confidence::Estimated));
        // Measured over this fixture: 17.1% of the time left in the first ten minutes.
        let early = worst_of_left(&ticks, 0, 600_000);
        check!(early < 0.20, "worst early error {early}");
        // From 40% of the pass on: 3.6% of the pass's length (8.2% of the time left then).
        let pass_ms = ticks[0].pass_ms;
        let late = ticks
            .iter()
            .filter(|t| t.at_ms * 10 >= pass_ms * 4)
            .filter_map(|t| shown_error(t, pass_ms))
            .fold(0.0, f64::max);
        check!(late < 0.05, "worst late error {late} of the pass");
    }

    #[test]
    fn bit_fade_no_longer_reads_as_nearly_done() {
        let shapes = shapes();
        let total: u64 = shapes.iter().map(Shape::work).sum();
        let before: u64 = shapes[..10].iter().map(Shape::work).sum();
        check!(before * 100 / total >= 99, "by words the pass is 99% done when bit fade starts");
        for priors in [priors(), vec![None; 11]] {
            let ticks = play(priors);
            let fade: Vec<&Tick> = ticks.iter().filter(|t| t.test == 10).collect();
            let first = fade[0];
            let left = first.forecast.raw.expect("a forecast").remaining;
            let spent = Duration::from_millis(first.at_ms);
            check!(
                spent.as_secs_f64() / (spent + left).as_secs_f64() < 0.80,
                "by time it is under 80%: {spent:?} spent, {left:?} left"
            );
            for tick in fade {
                let waits = match tick.forecast.position {
                    Position::Wait { nth, waits, left } => left + secs(120) * u32::try_from(waits - nth).unwrap(),
                    Position::Work { waits_passed, waits } => secs(120) * u32::try_from(waits - waits_passed).unwrap(),
                };
                let raw = tick.forecast.raw.expect("a forecast").remaining;
                check!(raw >= waits, "at {} s: {raw:?} left but {waits:?} of waits", tick.at_ms / 1000);
            }
        }
    }
}
