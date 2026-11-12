//! Loading pushed images, and the watchdog that bounds a run.

use ferrite_efi::link::receive::MAX_IMAGE;
use uefi::boot::{self, LoadImageSource};
use uefi::proto::device_path::{DevicePath, LoadedImageDevicePath};
use uefi::{Handle, Status};

/// Identifies the loader's watchdog arming to the firmware; codes above 0xffff are ours.
const WATCHDOG_CODE: u64 = 0x4645_5252;

const _: () = assert!(MAX_IMAGE <= u32::MAX as u64);

/// Load an image from memory. The firmware copies it, so `bytes` can be freed afterwards.
///
/// The image gets the loader's own device path as its file path, which is what lets it find the
/// boot volume it logs to.
pub fn load(bytes: &[u8]) -> uefi::Result<Handle> {
    let own = boot::open_protocol_exclusive::<LoadedImageDevicePath>(boot::image_handle())
        .inspect_err(|e| log::debug!("the loader's own device path is unavailable: {e}"))
        .ok();
    let file_path: Option<&DevicePath> = own.as_deref().map(|path| &**path);
    boot::load_image(boot::image_handle(), LoadImageSource::FromBuffer { buffer: bytes, file_path })
}

/// Unload an image that was loaded but never started, or that the firmware did not unload.
pub fn unload(image: Handle) {
    if let Err(e) = boot::unload_image(image) {
        log::debug!("unloading an image failed: {e}");
    }
}

/// Reset the machine if nothing disarms the watchdog within `secs`. `0` is refused by the caller
/// before it gets here, since to the firmware it means off.
pub fn arm(secs: u32) -> uefi::Result {
    let secs = usize::try_from(secs).map_err(|_| uefi::Error::from(Status::INVALID_PARAMETER))?;
    boot::set_watchdog_timer(secs, WATCHDOG_CODE, None)
}

/// Turn the watchdog off between runs and at startup, where the firmware may have left its own armed.
pub fn disarm() {
    if let Err(e) = boot::set_watchdog_timer(0, WATCHDOG_CODE, None) {
        log::warn!("disarming the watchdog failed: {e}");
    }
}

/// The status as the wire carries it.
pub fn code(status: Status) -> u64 {
    u64::try_from(status.0).unwrap_or(u64::MAX)
}
