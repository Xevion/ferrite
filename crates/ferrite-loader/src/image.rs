//! Loading pushed images.

use ferrite_efi::link::receive::MAX_IMAGE;
use uefi::boot::{self, LoadImageSource};
use uefi::proto::device_path::{DevicePath, LoadedImageDevicePath};
use uefi::{Handle, Status};

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

/// The status as the wire carries it.
pub fn code(status: Status) -> u64 {
    u64::try_from(status.0).unwrap_or(u64::MAX)
}
