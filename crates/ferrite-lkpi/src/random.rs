//! Kernel random bytes: a `ChaCha20` generator the app seeds once from the platform's entropy.
//!
//! Nothing here picks an entropy source, so the shim stays platform-free. A request before `seed` stops
//! the program like a stub hit, because falling back to a predictable stream would hide a missing seed.

use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, Ordering};

use rand_chacha::ChaCha20Rng;
use rand_core::{Rng, SeedableRng};
use snafu::Snafu;

use crate::trace_and_stop;

/// Bytes of platform entropy `seed` takes.
pub const SEED_LEN: usize = 32;

/// The generator, written once by `seed` and then only used from the one thread that runs the C.
struct Generator(UnsafeCell<Option<ChaCha20Rng>>);

// SAFETY: the C runs on one core without preemption, and firmware notifies run no Linux code, so the
// generator is never touched from two places at once.
unsafe impl Sync for Generator {}

static GENERATOR: Generator = Generator(UnsafeCell::new(None));
static SEEDED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Snafu)]
#[snafu(display("the kernel random generator is already seeded"))]
pub struct AlreadySeeded;

/// Seed the generator from `entropy`, once per program.
pub fn seed(entropy: [u8; SEED_LEN]) -> Result<(), AlreadySeeded> {
    if SEEDED.swap(true, Ordering::AcqRel) {
        return Err(AlreadySeeded);
    }
    // SAFETY: the flag above lets one caller in, and no request reads the cell until the flag is set
    // and this write is done, since the seeding thread is the only one running.
    unsafe { *GENERATOR.0.get() = Some(ChaCha20Rng::from_seed(entropy)) };
    Ok(())
}

/// Fill `buf` with random bytes.
///
/// # Safety
/// `buf` must be valid for `len` bytes of writes.
#[unsafe(no_mangle)]
pub unsafe extern "sysv64" fn lx_random_bytes(buf: *mut c_void, len: usize) {
    if !SEEDED.load(Ordering::Acquire) {
        log::error!("random bytes requested before the generator was seeded");
        trace_and_stop(c"lx_random_bytes");
    }
    // SAFETY: seeded, so the cell holds a generator, and nothing else uses it concurrently.
    let Some(rng) = (unsafe { &mut *GENERATOR.0.get() }) else { trace_and_stop(c"lx_random_bytes") };
    // SAFETY: the caller guarantees `buf` is writable for `len` bytes.
    rng.fill_bytes(unsafe { core::slice::from_raw_parts_mut(buf.cast(), len) });
}
