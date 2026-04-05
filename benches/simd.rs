use std::alloc::{Layout, alloc_zeroed, dealloc};

use divan::counter::BytesCount;
use divan::{Bencher, black_box};
use ferrite::Failure;

// 4 MiB / 64 MiB / 256 MiB / 512 MiB / 1 GiB / 2 GiB
const SIZES: [usize; 6] = [4 << 20, 64 << 20, 256 << 20, 512 << 20, 1 << 30, 2 << 30];

const PATTERN: u64 = 0xDEAD_BEEF_CAFE_BABEu64;

/// 64-byte aligned u64 buffer backed by the global allocator.
///
/// `fill_nt` requires 64-byte alignment to use NT stores rather than falling
/// back to scalar writes. Vec is typically 8-byte aligned, so we allocate
/// directly with the required alignment here.
struct AlignedBuffer {
    ptr: *mut u64,
    len: usize,
    layout: Layout,
}

impl AlignedBuffer {
    fn new(words: usize) -> Self {
        let layout = Layout::from_size_align(words * size_of::<u64>(), 64).expect("invalid layout");
        // SAFETY: layout has non-zero size (words > 0 for any bench size).
        // SAFETY: alloc_zeroed uses layout which guarantees 64-byte alignment — sufficient for u64 (8 bytes).
        #[allow(clippy::cast_ptr_alignment)]
        let ptr = unsafe { alloc_zeroed(layout).cast::<u64>() };
        assert!(!ptr.is_null(), "allocation failed");
        Self {
            ptr,
            len: words,
            layout,
        }
    }

    fn as_mut_slice(&mut self) -> &mut [u64] {
        // SAFETY: ptr is valid for `len` words, exclusively owned, and 64-byte aligned.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }

    fn as_slice(&self) -> &[u64] {
        // SAFETY: ptr is valid for `len` words and 64-byte aligned.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for AlignedBuffer {
    fn drop(&mut self) {
        // SAFETY: ptr was allocated with this layout.
        unsafe { dealloc(self.ptr.cast(), self.layout) }
    }
}

// SAFETY: AlignedBuffer owns its allocation exclusively; no aliasing.
unsafe impl Send for AlignedBuffer {}

trait FillBench {
    fn is_available() -> bool {
        true
    }
    unsafe fn fill_constant(buf: &mut [u64], pattern: u64);
    unsafe fn fill_indexed(buf: &mut [u64], start: usize);
}

trait VerifyBench {
    fn is_available() -> bool {
        true
    }
    unsafe fn verify_constant(buf: &[u64], pattern: u64, base_addr: usize) -> Vec<Failure>;
    unsafe fn verify_indexed(buf: &[u64], base_addr: usize) -> Vec<Failure>;
}

struct Scalar;

impl FillBench for Scalar {
    unsafe fn fill_constant(buf: &mut [u64], pattern: u64) {
        ferrite::bench_api::scalar_fill_constant(buf, pattern);
    }

    unsafe fn fill_indexed(buf: &mut [u64], start: usize) {
        ferrite::bench_api::scalar_fill_indexed(buf, start);
    }
}

impl VerifyBench for Scalar {
    unsafe fn verify_constant(buf: &[u64], pattern: u64, base_addr: usize) -> Vec<Failure> {
        ferrite::bench_api::scalar_verify_constant(buf, pattern, base_addr, 0)
    }

    unsafe fn verify_indexed(buf: &[u64], base_addr: usize) -> Vec<Failure> {
        ferrite::bench_api::scalar_verify_indexed(buf, base_addr, 0)
    }
}

#[cfg(target_arch = "x86_64")]
struct SimdAvx512;

#[cfg(target_arch = "x86_64")]
impl FillBench for SimdAvx512 {
    fn is_available() -> bool {
        ferrite::bench_api::avx512_available()
    }

    unsafe fn fill_constant(buf: &mut [u64], pattern: u64) {
        // SAFETY: caller must ensure AVX-512F is available (checked via is_available).
        unsafe { ferrite::bench_api::fill_nt(buf, pattern) }
    }

    unsafe fn fill_indexed(buf: &mut [u64], start: usize) {
        // SAFETY: caller must ensure AVX-512F is available (checked via is_available).
        unsafe { ferrite::bench_api::fill_nt_indexed(buf, start) }
    }
}

#[cfg(target_arch = "x86_64")]
impl VerifyBench for SimdAvx512 {
    fn is_available() -> bool {
        ferrite::bench_api::avx512_available()
    }

    unsafe fn verify_constant(buf: &[u64], pattern: u64, base_addr: usize) -> Vec<Failure> {
        // SAFETY: caller must ensure AVX-512F is available (checked via is_available).
        unsafe { ferrite::bench_api::verify_avx512(buf, pattern, base_addr, 0) }
    }

    unsafe fn verify_indexed(buf: &[u64], base_addr: usize) -> Vec<Failure> {
        // SAFETY: caller must ensure AVX-512F is available (checked via is_available).
        unsafe { ferrite::bench_api::verify_indexed_avx512(buf, base_addr, 0) }
    }
}

#[cfg(target_arch = "x86_64")]
#[divan::bench(types = [Scalar, SimdAvx512], args = SIZES)]
fn constant_fill<W: FillBench>(bencher: Bencher, bytes: usize) {
    if !W::is_available() {
        return;
    }
    let mut buf = AlignedBuffer::new(bytes / size_of::<u64>());
    bencher
        .counter(BytesCount::new(bytes as u64))
        .bench_local(|| {
            // SAFETY: is_available() checked above; buffer is 64-byte aligned.
            unsafe { W::fill_constant(buf.as_mut_slice(), black_box(PATTERN)) }
        });
}

#[cfg(not(target_arch = "x86_64"))]
#[divan::bench(args = SIZES)]
fn constant_fill_scalar(bencher: Bencher, bytes: usize) {
    let mut buf = AlignedBuffer::new(bytes / size_of::<u64>());
    bencher
        .counter(BytesCount::new(bytes as u64))
        .bench_local(|| unsafe { Scalar::fill_constant(buf.as_mut_slice(), black_box(PATTERN)) });
}

#[cfg(target_arch = "x86_64")]
#[divan::bench(types = [Scalar, SimdAvx512], args = SIZES)]
fn indexed_fill<W: FillBench>(bencher: Bencher, bytes: usize) {
    if !W::is_available() {
        return;
    }
    let mut buf = AlignedBuffer::new(bytes / size_of::<u64>());
    bencher
        .counter(BytesCount::new(bytes as u64))
        .bench_local(|| {
            // SAFETY: is_available() checked above; buffer is 64-byte aligned.
            unsafe { W::fill_indexed(buf.as_mut_slice(), black_box(0usize)) }
        });
}

#[cfg(not(target_arch = "x86_64"))]
#[divan::bench(args = SIZES)]
fn indexed_fill_scalar(bencher: Bencher, bytes: usize) {
    let mut buf = AlignedBuffer::new(bytes / size_of::<u64>());
    bencher
        .counter(BytesCount::new(bytes as u64))
        .bench_local(|| unsafe { Scalar::fill_indexed(buf.as_mut_slice(), black_box(0usize)) });
}

#[cfg(target_arch = "x86_64")]
#[divan::bench(types = [Scalar, SimdAvx512], args = SIZES)]
fn constant_verify<W: VerifyBench + FillBench>(bencher: Bencher, bytes: usize) {
    if !<W as VerifyBench>::is_available() {
        return;
    }
    let mut buf = AlignedBuffer::new(bytes / size_of::<u64>());
    // Pre-fill so verify finds no mismatches (happy path = production path).
    unsafe { W::fill_constant(buf.as_mut_slice(), PATTERN) };
    let base_addr = buf.ptr as usize;
    bencher
        .counter(BytesCount::new(bytes as u64))
        .bench_local(|| {
            // SAFETY: is_available() checked above; buffer is 64-byte aligned and pre-filled.
            unsafe { W::verify_constant(black_box(buf.as_slice()), PATTERN, base_addr) }
        });
}

#[cfg(not(target_arch = "x86_64"))]
#[divan::bench(args = SIZES)]
fn constant_verify_scalar(bencher: Bencher, bytes: usize) {
    let mut buf = AlignedBuffer::new(bytes / size_of::<u64>());
    unsafe { Scalar::fill_constant(buf.as_mut_slice(), PATTERN) };
    let base_addr = buf.ptr as usize;
    bencher
        .counter(BytesCount::new(bytes as u64))
        .bench_local(|| unsafe {
            Scalar::verify_constant(black_box(buf.as_slice()), PATTERN, base_addr)
        });
}

#[cfg(target_arch = "x86_64")]
#[divan::bench(types = [Scalar, SimdAvx512], args = SIZES)]
fn indexed_verify<W: VerifyBench + FillBench>(bencher: Bencher, bytes: usize) {
    if !<W as VerifyBench>::is_available() {
        return;
    }
    let mut buf = AlignedBuffer::new(bytes / size_of::<u64>());
    unsafe { W::fill_indexed(buf.as_mut_slice(), 0) };
    let base_addr = buf.ptr as usize;
    bencher
        .counter(BytesCount::new(bytes as u64))
        .bench_local(|| {
            // SAFETY: is_available() checked above; buffer is 64-byte aligned and pre-filled.
            unsafe { W::verify_indexed(black_box(buf.as_slice()), base_addr) }
        });
}

#[cfg(not(target_arch = "x86_64"))]
#[divan::bench(args = SIZES)]
fn indexed_verify_scalar(bencher: Bencher, bytes: usize) {
    let mut buf = AlignedBuffer::new(bytes / size_of::<u64>());
    unsafe { Scalar::fill_indexed(buf.as_mut_slice(), 0) };
    let base_addr = buf.ptr as usize;
    bencher
        .counter(BytesCount::new(bytes as u64))
        .bench_local(|| unsafe { Scalar::verify_indexed(black_box(buf.as_slice()), base_addr) });
}

fn main() {
    divan::main();
}
