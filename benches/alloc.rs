use divan::counter::BytesCount;
use divan::{Bencher, black_box};
use ferrite::alloc::TestBuffer;

mod common;
use common::Size;

const SIZES: [Size; 3] = [Size(4 << 20), Size(64 << 20), Size(256 << 20)];

/// Full mmap → page-fault → mlock → munmap cycle at each size. Only `mlock` is
/// privileged (`CAP_IPC_LOCK` or a large enough `RLIMIT_MEMLOCK`), so probe by
/// attempting one alloc and skip only the sizes that actually fail to lock.
#[divan::bench(args = SIZES)]
fn alloc_lock_free(bencher: Bencher, size: Size) {
    let bytes = size.bytes();
    match TestBuffer::new(bytes) {
        Ok(region) => drop(region),
        Err(e) => {
            eprintln!("[alloc bench] skipped {size}: {e}");
            return;
        }
    }
    bencher
        .counter(BytesCount::new(bytes as u64))
        .bench_local(|| {
            let region = TestBuffer::new(black_box(bytes)).expect("TestBuffer::new failed");
            drop(region);
        });
}

fn main() {
    divan::main();
}
