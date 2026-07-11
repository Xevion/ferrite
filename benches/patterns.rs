use divan::Bencher;
use divan::counter::BytesCount;
use ferrite::pattern::{Pattern, PatternConfig, run_pattern};

mod common;
use common::Size;

const SIZES: [Size; 3] = [Size(4 << 20), Size(64 << 20), Size(256 << 20)];

#[divan::bench(args = SIZES)]
fn solid_bits(bencher: Bencher, size: Size) {
    let bytes = size.bytes();
    let mut buf = vec![0u64; bytes / 8];
    bencher
        .counter(BytesCount::new(bytes as u64))
        .bench_local(|| {
            run_pattern(
                Pattern::SolidBits,
                &mut buf,
                false,
                &PatternConfig::default(),
                &ferrite::FailureBudget::unlimited(),
                &mut || {},
                &|_| {},
            )
        });
}

#[divan::bench(args = SIZES)]
fn walking_ones(bencher: Bencher, size: Size) {
    let bytes = size.bytes();
    let mut buf = vec![0u64; bytes / 8];
    bencher
        .counter(BytesCount::new(bytes as u64))
        .bench_local(|| {
            run_pattern(
                Pattern::WalkingOnes,
                &mut buf,
                false,
                &PatternConfig::default(),
                &ferrite::FailureBudget::unlimited(),
                &mut || {},
                &|_| {},
            )
        });
}

#[divan::bench(args = SIZES)]
fn walking_zeros(bencher: Bencher, size: Size) {
    let bytes = size.bytes();
    let mut buf = vec![0u64; bytes / 8];
    bencher
        .counter(BytesCount::new(bytes as u64))
        .bench_local(|| {
            run_pattern(
                Pattern::WalkingZeros,
                &mut buf,
                false,
                &PatternConfig::default(),
                &ferrite::FailureBudget::unlimited(),
                &mut || {},
                &|_| {},
            )
        });
}

#[divan::bench(args = SIZES)]
fn checkerboard(bencher: Bencher, size: Size) {
    let bytes = size.bytes();
    let mut buf = vec![0u64; bytes / 8];
    bencher
        .counter(BytesCount::new(bytes as u64))
        .bench_local(|| {
            run_pattern(
                Pattern::Checkerboard,
                &mut buf,
                false,
                &PatternConfig::default(),
                &ferrite::FailureBudget::unlimited(),
                &mut || {},
                &|_| {},
            )
        });
}

#[divan::bench(args = SIZES)]
fn stuck_address(bencher: Bencher, size: Size) {
    let bytes = size.bytes();
    let mut buf = vec![0u64; bytes / 8];
    bencher
        .counter(BytesCount::new(bytes as u64))
        .bench_local(|| {
            run_pattern(
                Pattern::StuckAddress,
                &mut buf,
                false,
                &PatternConfig::default(),
                &ferrite::FailureBudget::unlimited(),
                &mut || {},
                &|_| {},
            )
        });
}

fn main() {
    divan::main();
}
