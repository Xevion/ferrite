use divan::Bencher;
use divan::counter::BytesCount;
use ferrite::pattern::{Pattern, run_pattern};

const SIZES: [usize; 3] = [4 << 20, 64 << 20, 256 << 20]; // 4 / 64 / 256 MiB

#[divan::bench(args = SIZES)]
fn solid_bits(bencher: Bencher, bytes: usize) {
    let mut buf = vec![0u64; bytes / 8];
    bencher
        .counter(BytesCount::new(bytes as u64))
        .bench_local(|| run_pattern(Pattern::SolidBits, &mut buf, false, &mut || {}, &|_| {}));
}

#[divan::bench(args = SIZES)]
fn walking_ones(bencher: Bencher, bytes: usize) {
    let mut buf = vec![0u64; bytes / 8];
    bencher
        .counter(BytesCount::new(bytes as u64))
        .bench_local(|| run_pattern(Pattern::WalkingOnes, &mut buf, false, &mut || {}, &|_| {}));
}

#[divan::bench(args = SIZES)]
fn walking_zeros(bencher: Bencher, bytes: usize) {
    let mut buf = vec![0u64; bytes / 8];
    bencher
        .counter(BytesCount::new(bytes as u64))
        .bench_local(|| run_pattern(Pattern::WalkingZeros, &mut buf, false, &mut || {}, &|_| {}));
}

#[divan::bench(args = SIZES)]
fn checkerboard(bencher: Bencher, bytes: usize) {
    let mut buf = vec![0u64; bytes / 8];
    bencher
        .counter(BytesCount::new(bytes as u64))
        .bench_local(|| run_pattern(Pattern::Checkerboard, &mut buf, false, &mut || {}, &|_| {}));
}

#[divan::bench(args = SIZES)]
fn stuck_address(bencher: Bencher, bytes: usize) {
    let mut buf = vec![0u64; bytes / 8];
    bencher
        .counter(BytesCount::new(bytes as u64))
        .bench_local(|| run_pattern(Pattern::StuckAddress, &mut buf, false, &mut || {}, &|_| {}));
}

fn main() {
    divan::main();
}
