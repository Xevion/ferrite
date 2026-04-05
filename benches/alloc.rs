use divan::counter::BytesCount;
use divan::{Bencher, black_box};
use ferrite::alloc::LockedRegion;

const SIZES: [usize; 3] = [4 << 20, 64 << 20, 256 << 20]; // 4 / 64 / 256 MiB

fn is_root() -> bool {
    nix::unistd::getuid().is_root()
}

/// Full mmap → page-fault → mlock → munmap cycle at each size.
/// Skipped silently when not running as root (mlock requires `CAP_IPC_LOCK`).
#[divan::bench(args = SIZES)]
fn alloc_lock_free(bencher: Bencher, bytes: usize) {
    if !is_root() {
        eprintln!("[alloc bench] skipped (requires root for mlock)");
        return;
    }
    bencher
        .counter(BytesCount::new(bytes as u64))
        .bench_local(|| {
            let region = LockedRegion::new(black_box(bytes)).expect("LockedRegion::new failed");
            drop(region);
        });
}

fn main() {
    divan::main();
}
