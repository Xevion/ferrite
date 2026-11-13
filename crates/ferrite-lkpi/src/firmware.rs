//! Firmware files for the shadow firmware loader: the C asks for a file by name, and a provider the
//! app installs supplies its bytes. Nothing here knows where files live, so the shim stays platform-free.
//!
//! The buffer a successful load returns belongs to the shim until the C hands it back through
//! `lx_firmware_free`, which `release_firmware` does.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ffi::{CStr, c_char, c_int};
use core::sync::atomic::{AtomicPtr, Ordering};

use snafu::Snafu;

use crate::trace_and_stop;

const ENOENT: c_int = 2;
const EIO: c_int = 5;
const EINVAL: c_int = 22;

#[derive(Debug, Snafu)]
pub enum FirmwareError {
    /// No file of that name; the C sees `-ENOENT`, which drivers handle.
    #[snafu(display("no firmware named {name}"))]
    NotFound { name: alloc::string::String },
    /// The file exists but could not be read; the C sees `-EIO`.
    #[snafu(display("firmware {name} could not be read: {reason}"))]
    Unreadable { name: alloc::string::String, reason: alloc::string::String },
}

/// Where firmware files come from on this platform.
pub trait FirmwareProvider: Sync {
    /// The bytes of the file `name`, a Linux firmware name such as `rtw89/rtw8852c_fw-2.bin`.
    fn load(&self, name: &str) -> Result<Vec<u8>, FirmwareError>;
}

/// Zero while unset, otherwise a leaked `&'static &'static dyn FirmwareProvider`.
static PROVIDER: AtomicPtr<&'static dyn FirmwareProvider> = AtomicPtr::new(core::ptr::null_mut());

#[derive(Debug, Snafu)]
#[snafu(display("a firmware provider is already installed"))]
pub struct ProviderAlreadyInstalled;

/// Install the app's provider, once per program.
pub fn install(provider: &'static dyn FirmwareProvider) -> Result<(), ProviderAlreadyInstalled> {
    let cell = Box::into_raw(Box::new(provider));
    if PROVIDER.compare_exchange(core::ptr::null_mut(), cell, Ordering::AcqRel, Ordering::Acquire).is_err() {
        // SAFETY: the box was never shared.
        drop(unsafe { Box::from_raw(cell) });
        return Err(ProviderAlreadyInstalled);
    }
    Ok(())
}

fn provider() -> &'static dyn FirmwareProvider {
    let cell = PROVIDER.load(Ordering::Acquire);
    if cell.is_null() {
        log::error!("firmware requested before a provider was installed");
        trace_and_stop(c"lx_firmware_load");
    }
    // SAFETY: set once by `install` from a leaked box, never freed.
    unsafe { *cell }
}

fn load(name: &CStr) -> Result<Box<[u8]>, c_int> {
    let Ok(name) = name.to_str() else { return Err(-EINVAL) };
    match provider().load(name) {
        // Linux's kernel_read_file refuses an empty file the same way.
        Ok(bytes) if bytes.is_empty() => Err(-EINVAL),
        Ok(bytes) => {
            log::info!("firmware request {name}: {} bytes", bytes.len());
            Ok(bytes.into_boxed_slice())
        },
        Err(FirmwareError::NotFound { .. }) => {
            log::info!("firmware request {name}: not found");
            Err(-ENOENT)
        },
        Err(error) => {
            log::warn!("{error}");
            Err(-EIO)
        },
    }
}

/// Load the firmware `name`. On success returns 0 and sets `*data` and `*size`; otherwise a negative
/// errno (`-ENOENT` when there is no such file) and the outputs are untouched.
///
/// # Safety
/// `name` must be a NUL-terminated string, and `data` and `size` valid for writes.
#[unsafe(no_mangle)]
pub unsafe extern "sysv64" fn lx_firmware_load(name: *const c_char, data: *mut *mut u8, size: *mut usize) -> c_int {
    // SAFETY: the caller guarantees a NUL-terminated string.
    let name = unsafe { CStr::from_ptr(name) };
    match load(name) {
        Ok(bytes) => {
            let len = bytes.len();
            // SAFETY: the caller guarantees both outputs are writable.
            unsafe {
                data.write(Box::into_raw(bytes).cast());
                size.write(len);
            }
            0
        },
        Err(errno) => errno,
    }
}

/// Free a buffer `lx_firmware_load` returned.
///
/// # Safety
/// `data` and `size` must be exactly what one successful `lx_firmware_load` set, freed once.
#[unsafe(no_mangle)]
pub unsafe extern "sysv64" fn lx_firmware_free(data: *mut u8, size: usize) {
    // SAFETY: the caller returns the pointer and length of a boxed slice made by `lx_firmware_load`.
    drop(unsafe { Box::from_raw(core::ptr::slice_from_raw_parts_mut(data, size)) });
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString as _;
    use alloc::vec;
    use core::ptr::null_mut;

    use assert2::check;

    use super::*;

    struct Fake;

    impl FirmwareProvider for Fake {
        fn load(&self, name: &str) -> Result<Vec<u8>, FirmwareError> {
            match name {
                "a.bin" => Ok(vec![1, 2, 3]),
                "empty.bin" => Ok(vec![]),
                "broken.bin" => Err(FirmwareError::Unreadable { name: name.to_string(), reason: "disk".to_string() }),
                _ => Err(FirmwareError::NotFound { name: name.to_string() }),
            }
        }
    }

    #[test]
    fn load_maps_provider_results_to_errno_and_frees_the_buffer() {
        static FAKE: Fake = Fake;
        check!(install(&FAKE).is_ok());
        check!(install(&FAKE).is_err());

        let (mut data, mut size) = (null_mut(), 0);
        // SAFETY: valid NUL-terminated names and writable outputs.
        unsafe {
            check!(lx_firmware_load(c"a.bin".as_ptr(), &raw mut data, &raw mut size) == 0);
            check!(core::slice::from_raw_parts(data, size) == [1, 2, 3]);
            lx_firmware_free(data, size);
            check!(lx_firmware_load(c"nope.bin".as_ptr(), &raw mut data, &raw mut size) == -ENOENT);
            check!(lx_firmware_load(c"broken.bin".as_ptr(), &raw mut data, &raw mut size) == -EIO);
            check!(lx_firmware_load(c"empty.bin".as_ptr(), &raw mut data, &raw mut size) == -EINVAL);
        }
    }
}
