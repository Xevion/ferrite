//! Host timings of the kernels and of the slow tests, run by hand:
//! `cargo nextest run -p ferrite-mem --run-ignored only -E 'test(bench)' --no-capture`.
//! `FERRITE_BENCH_MIB` sets the buffer (default 1024), `FERRITE_BENCH_THREADS` the threads (default 1),
//! `FERRITE_BENCH_REPS` the repeats, `FERRITE_BENCH_ONLY` a comma-separated list of stages (or, for
//! the tests, kernel configs) to run, `FERRITE_BENCH_KINDS` a substring of the test names.
#![expect(
    clippy::cast_precision_loss,
    clippy::format_collect,
    clippy::format_push_string,
    reason = "a host timing report"
)]

use std::eprintln;
use std::string::String;
use std::time::Instant as Clock;
use std::vec::Vec;

use assert2::assert;
use ferrite_hal::Instant;

use super::*;
use crate::kernels::{Stride, fill_stride, verify_fill_stride, verify_stride};
use crate::ops::fill_except;
use crate::suite::{Config, Kernels, Progress, Test, TestKind};

const CHUNK_BYTES: usize = WIDE_CHUNK_BYTES;

fn env(name: &str, default: usize) -> usize {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

#[derive(Clone, Copy)]
struct SendPtr(*mut u64);

// SAFETY: each thread gets its own disjoint slice of the buffer.
unsafe impl Send for SendPtr {}

/// Run `work` over `threads` disjoint slices of `buf`, each on its own thread; the wall time.
fn parallel(buf: &mut Buf, threads: usize, work: impl Fn(&mut Region<'_>, usize) + Sync) -> f64 {
    let words = buf.words;
    let per = (words / threads) / BLOCK_WORDS * BLOCK_WORDS;
    let base = SendPtr(buf.region().ptr());
    let start = Clock::now();
    std::thread::scope(|s| {
        for t in 0..threads {
            let work = &work;
            s.spawn(move || {
                let base = base;
                let len = if t + 1 == threads { words - per * t } else { per };
                // SAFETY: the slices are disjoint, block aligned, and inside the buffer.
                let mut region = unsafe { Region::from_raw(base.0.add(t * per), len) }.unwrap();
                work(&mut region, t * per);
            });
        }
    });
    start.elapsed().as_secs_f64()
}

/// The sequence states a stage steps from chunk to chunk.
struct Seq {
    prsg: Prsg,
    rot: Rotating,
}

/// One timed operation, run chunk by chunk; `first` is the chunk's first word in the whole buffer.
struct Stage {
    name: &'static str,
    descending: bool,
    run: fn(Kernel, &mut Region<'_>, &mut Seq, usize, &mut Faults<'_>),
}

const SEED: u64 = 1;
const PATTERN: u64 = 0x5555_aaaa_5555_aaaa;
const SHIFT: u64 = 1 << 5;
const UP: Inversion = Inversion { expect: 0, write: !0, order: Order::Ascending };
const DOWN: Inversion = Inversion { expect: !0, write: 0, order: Order::Descending };
const STRIDE_VALUE: u64 = 0x1234_5678_9abc_def0;

fn stride(first: usize) -> Stride {
    Stride { n: 20, offset: 7, first: first % 20 }
}

/// The stages in an order that leaves memory holding what the next one expects.
fn stages() -> Vec<Stage> {
    let st = |name, descending, run| Stage { name, descending, run };
    vec![
        st("fill", false, |k, c, _, _, _| fill(k, c, PATTERN)),
        st("verify", false, |k, c, _, _, f| verify(k, c, PATTERN, f)),
        st("invert up", false, |k, c, _, _, f| {
            invert(k, c, Inversion { expect: PATTERN, write: !PATTERN, order: Order::Ascending }, f);
        }),
        st("invert down", true, |k, c, _, _, f| {
            invert(k, c, Inversion { expect: !PATTERN, write: PATTERN, order: Order::Descending }, f);
        }),
        st("fill_prsg", false, |k, c, s, _, _| fill_prsg(k, c, &mut s.prsg, 0)),
        st("invert_prsg up", false, |k, c, s, _, f| invert_prsg(k, c, &mut s.prsg, UP, f)),
        st("invert_prsg down", true, |k, c, s, _, f| invert_prsg(k, c, &mut s.prsg, DOWN, f)),
        st("fill_rotating", false, |k, c, s, _, _| fill_rotating(k, c, &mut s.rot, 0)),
        st("invert_rotating up", false, |k, c, s, _, f| invert_rotating(k, c, &mut s.rot, UP, f)),
        st("invert_rotating down", true, |k, c, s, _, f| invert_rotating(k, c, &mut s.rot, DOWN, f)),
        st("fill_stride", false, |_, c, _, first, _| {
            // SAFETY: the chunk is a region.
            unsafe { fill_stride(c.ptr(), c.words(), stride(first), STRIDE_VALUE) };
        }),
        st("fill_except", false, |k, c, _, first, _| fill_except(k, c, stride(first), !STRIDE_VALUE)),
        st("verify_stride", false, |_, c, _, first, f| {
            // SAFETY: the chunk is a region.
            unsafe { verify_stride(c.ptr(), c.words(), stride(first), STRIDE_VALUE, f) };
        }),
        st("verify_fill_stride", false, |_, c, _, first, f| {
            // SAFETY: the chunk is a region.
            unsafe {
                verify_fill_stride(c.ptr(), c.words(), stride(first), STRIDE_VALUE, stride(first), !STRIDE_VALUE, f);
            };
        }),
    ]
}

/// Run `stage` over one slice, in chunks of `CHUNK_BYTES`, in its direction.
fn walk(region: &mut Region<'_>, kernel: Kernel, base: usize, faults: &mut Faults<'_>, stage: &Stage) {
    let chunk = CHUNK_BYTES / 8;
    let blocks = (region.words() / BLOCK_WORDS) as u64;
    let mut seq = Seq {
        prsg: if stage.descending { Prsg::at(SEED, blocks) } else { Prsg::new(SEED) },
        rot: Rotating::at(SHIFT, if stage.descending { blocks } else { 0 }),
    };
    let words = region.words();
    let starts: Vec<usize> = (0..words).step_by(chunk).collect();
    let order: Vec<usize> = if stage.descending { starts.into_iter().rev().collect() } else { starts };
    for at in order {
        let mut c = region.sub(at, chunk);
        (stage.run)(kernel, &mut c, &mut seq, base + at, faults);
    }
}

fn gbps(bytes: usize, secs: f64) -> f64 {
    bytes as f64 / secs / 1e9
}

#[test]
#[ignore = "benchmark"]
fn bench_ops() {
    let mib = env("FERRITE_BENCH_MIB", 1024);
    let threads = env("FERRITE_BENCH_THREADS", 1);
    let reps = env("FERRITE_BENCH_REPS", 3);
    let words = mib << 17;
    let mut buf = Buf::new(words);
    parallel(&mut buf, threads, |r, _| fill(Kernel::SCALAR, r, 1));
    eprintln!("ops over {mib} MiB, {threads} thread(s), best of {reps}; GB/s of region per pass");
    let only = std::env::var("FERRITE_BENCH_ONLY").unwrap_or_default();
    let stages: Vec<Stage> =
        stages().into_iter().filter(|s| only.is_empty() || only.split(',').any(|o| o == s.name)).collect();
    let mut best = std::vec![std::vec![f64::MAX; kernels().len()]; stages.len()];
    for _ in 0..reps {
        for (ki, kernel) in kernels().into_iter().enumerate() {
            for (si, stage) in stages.iter().enumerate() {
                let secs = parallel(&mut buf, threads, |r, base| {
                    let mut sink = |_| {};
                    let mut faults = Faults::new(0, &mut sink);
                    walk(r, kernel, base, &mut faults, stage);
                    assert!(faults.seen() == 0, "{} on {kernel}", stage.name);
                });
                best[si][ki] = best[si][ki].min(secs);
            }
        }
    }
    eprintln!("{:>22} {}", "", kernels().iter().map(|k| std::format!("{k:>10}")).collect::<String>());
    for (si, stage) in stages.iter().enumerate() {
        let row: String = best[si].iter().map(|&s| std::format!("{:>10.1}", gbps(words * 8, s))).collect();
        eprintln!("{:>22} {row}", stage.name);
    }
}

/// A config with counts small enough to time a 1 GiB buffer, scaled in the report.
const BENCH: Config = Config {
    random_rounds: 8,
    modulo_rounds: 1,
    byte_iterations: 3,
    shifting_iterations: 1,
    fixed_iterations: 6,
    ..Config::DEFAULT
};

/// Memtest86+'s counts, which the report scales the bench config's seconds to.
fn scale(kind: TestKind) -> f64 {
    match kind {
        TestKind::MovingInversionsRandom => 128.0 / f64::from(BENCH.random_rounds),
        TestKind::Modulo20 => 8.0,
        _ => 1.0,
    }
}

fn run_test(kind: TestKind, kernels: Kernels, buf: &mut Buf, threads: usize) -> f64 {
    parallel(buf, threads, |region, _| {
        let mut sink = |_| {};
        let mut faults = Faults::new(0, &mut sink);
        let mut test = Test::new(kind, BENCH, 0, region);
        let now = Instant::default();
        while test.step(region, &kernels, CHUNK_BYTES, now, &mut faults) != Progress::Done {}
        assert!(faults.seen() == 0, "{kind:?} on clean memory");
    })
}

#[test]
#[ignore = "benchmark"]
fn bench_tests() {
    let mib = env("FERRITE_BENCH_MIB", 1024);
    let threads = env("FERRITE_BENCH_THREADS", 1);
    let reps = env("FERRITE_BENCH_REPS", 1);
    let words = mib << 17;
    let mut buf = Buf::new(words);
    parallel(&mut buf, threads, |r, _| fill(Kernel::SCALAR, r, 1));
    // SAFETY: the flags come from the host's own feature detection.
    let caps = unsafe { Caps::new(std::is_x86_feature_detected!("avx2"), std::is_x86_feature_detected!("avx512f")) };
    let mut configs: Vec<(String, Kernels)> = vec![(String::from("for_caps"), Kernels::for_caps(caps))];
    configs.extend(
        kernels()
            .into_iter()
            .filter(|k| matches!(k.stores(), Stores::Temporal))
            .map(|k| (std::format!("{k}"), Kernels::uniform(k))),
    );
    let only = std::env::var("FERRITE_BENCH_ONLY").unwrap_or_default();
    let configs: Vec<_> =
        configs.into_iter().filter(|(n, _)| only.is_empty() || only.split(',').any(|o| o == n)).collect();
    let kinds = [
        TestKind::Modulo20,
        TestKind::MovingInversionsRandom,
        TestKind::MovingInversions64Bit,
        TestKind::MovingInversions8Bit,
        TestKind::MovingInversions,
    ];
    let which = std::env::var("FERRITE_BENCH_KINDS").unwrap_or_default();
    let kinds = kinds.into_iter().filter(|k| which.is_empty() || k.name().contains(&which));
    eprintln!(
        "tests over {mib} MiB, {threads} thread(s); seconds, scaled to memtest86+ counts (x{{scale}}) in brackets"
    );
    for kind in kinds {
        let mut line = String::new();
        for (name, kernels) in &configs {
            let best = (0..reps).map(|_| run_test(kind, *kernels, &mut buf, threads)).fold(f64::MAX, f64::min);
            line += &std::format!(" {name}={best:.2}s[{:.0}]", best * scale(kind));
        }
        eprintln!("{:>38}:{line}", kind.name());
    }
}
