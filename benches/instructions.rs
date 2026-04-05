use gungraun::{library_benchmark, library_benchmark_group, main};

/// One CHUNK = 64K u64 words = 512 KiB. Matches `simd::CHUNK` so instruction
/// counts reflect the actual rayon-task granularity used in production.
const BENCH_WORDS: usize = 64 * 1024;
const BENCH_PATTERN: u64 = 0xAAAA_AAAA_AAAA_AAAAu64;

fn setup_buf() -> Vec<u64> {
    vec![0u64; BENCH_WORDS]
}

#[library_benchmark]
#[bench::single(setup = setup_buf)]
fn scalar_fill_constant(mut buf: Vec<u64>) -> Vec<u64> {
    ferrite::bench_api::scalar_fill_constant(&mut buf, BENCH_PATTERN);
    buf
}

#[library_benchmark]
#[bench::single(setup = setup_buf)]
fn scalar_fill_indexed(mut buf: Vec<u64>) -> Vec<u64> {
    ferrite::bench_api::scalar_fill_indexed(&mut buf, 0);
    buf
}

#[library_benchmark]
#[bench::single(setup = setup_buf)]
fn scalar_verify_constant(buf: Vec<u64>) -> Vec<u64> {
    let base = buf.as_ptr() as usize;
    let _failures = ferrite::bench_api::scalar_verify_constant(&buf, BENCH_PATTERN, base, 0);
    buf
}

#[cfg(target_arch = "x86_64")]
#[library_benchmark]
#[bench::single(setup = setup_buf)]
fn simd_fill_nt(mut buf: Vec<u64>) -> Vec<u64> {
    if !ferrite::bench_api::avx512_available() {
        eprintln!("[simd_fill_nt] skipped — AVX-512 not detected (valgrind does not emulate it)");
        return buf;
    }
    // SAFETY: avx512_available() checked above.
    unsafe { ferrite::bench_api::fill_nt(&mut buf, BENCH_PATTERN) };
    buf
}

#[cfg(target_arch = "x86_64")]
#[library_benchmark]
#[bench::single(setup = setup_buf)]
fn simd_fill_nt_indexed(mut buf: Vec<u64>) -> Vec<u64> {
    if !ferrite::bench_api::avx512_available() {
        eprintln!(
            "[simd_fill_nt_indexed] skipped — AVX-512 not detected (valgrind does not emulate it)"
        );
        return buf;
    }
    // SAFETY: avx512_available() checked above.
    unsafe { ferrite::bench_api::fill_nt_indexed(&mut buf, 0) };
    buf
}

#[cfg(target_arch = "x86_64")]
#[library_benchmark]
#[bench::single(setup = setup_buf)]
fn simd_verify_avx512(buf: Vec<u64>) -> Vec<u64> {
    if !ferrite::bench_api::avx512_available() {
        eprintln!(
            "[simd_verify_avx512] skipped — AVX-512 not detected (valgrind does not emulate it)"
        );
        return buf;
    }
    let base = buf.as_ptr() as usize;
    // SAFETY: avx512_available() checked above.
    let _failures = unsafe { ferrite::bench_api::verify_avx512(&buf, BENCH_PATTERN, base, 0) };
    buf
}

fn setup_pagemap_buf() -> Vec<u64> {
    // Allocate and touch all pages so they're present in pagemap before measurement.
    let mut buf = vec![0u64; BENCH_WORDS];
    buf.fill(1);
    buf
}

#[library_benchmark]
#[bench::single(setup = setup_pagemap_buf)]
fn phys_build_map(buf: Vec<u64>) -> Vec<u64> {
    use ferrite::phys::{PagemapResolver, PhysResolver};
    let base = buf.as_ptr() as usize;
    let len = buf.len() * size_of::<u64>();
    if let Ok(mut resolver) = PagemapResolver::new() {
        let _ = resolver.build_map(base, len);
    }
    buf
}

library_benchmark_group!(
    name = scalar_patterns;
    benchmarks = scalar_fill_constant, scalar_fill_indexed, scalar_verify_constant
);

#[cfg(target_arch = "x86_64")]
library_benchmark_group!(
    name = simd_ops;
    benchmarks = simd_fill_nt, simd_fill_nt_indexed, simd_verify_avx512
);

library_benchmark_group!(
    name = phys_resolution;
    benchmarks = phys_build_map
);

#[cfg(target_arch = "x86_64")]
main!(
    library_benchmark_groups = scalar_patterns,
    simd_ops,
    phys_resolution
);

#[cfg(not(target_arch = "x86_64"))]
main!(library_benchmark_groups = scalar_patterns, phys_resolution);
