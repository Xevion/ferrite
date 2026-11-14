//! The volume this image was loaded from: opening it shared, and reading files below a directory on it.

use alloc::string::{String, ToString as _};
use alloc::vec::Vec;
use core::mem::ManuallyDrop;

use snafu::Snafu;
use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams, ScopedProtocol};
use uefi::proto::device_path::DevicePath;
use uefi::proto::loaded_image::LoadedImage;
use uefi::proto::media::file::{File, FileAttribute, FileInfo, FileMode};
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::{CString16, Status};

/// The file system this image was loaded from, opened shared.
///
/// `boot::get_image_file_system` opens it exclusively, which a loader holding its own log
/// would then refuse to every image it starts. The open is never closed, because some firmware
/// does not track a `GetProtocol` open and fails the close.
pub fn image_volume() -> uefi::Result<ManuallyDrop<ScopedProtocol<SimpleFileSystem>>> {
    let image = boot::image_handle();
    let device = boot::open_protocol_exclusive::<LoadedImage>(image)?
        .device()
        .ok_or_else(|| uefi::Error::from(Status::UNSUPPORTED))?;
    let path = boot::open_protocol_exclusive::<DevicePath>(device)?;
    let handle = boot::locate_device_path::<SimpleFileSystem>(&mut &*path)?;
    let params = OpenProtocolParams { handle, agent: image, controller: None };
    // SAFETY: a shared open of a file system; every file operation goes through its own handle.
    let volume = unsafe { boot::open_protocol::<SimpleFileSystem>(params, OpenProtocolAttributes::GetProtocol) }?;
    Ok(ManuallyDrop::new(volume))
}

/// The volume path of `name`, a `/`-separated relative path, below `dir` (a volume path with `\`
/// separators).
///
/// `None` unless every component is a plain name: no empty, `.` or `..` component, no `\` or `:`, so
/// the result cannot leave `dir`.
#[must_use]
pub fn confined_path(dir: &str, name: &str) -> Option<String> {
    let plain = |part: &str| !part.is_empty() && part != "." && part != ".." && !part.contains(['\\', ':']);
    if !name.split('/').all(plain) {
        return None;
    }
    let mut path = String::from(dir);
    for part in name.split('/') {
        path.push('\\');
        path.push_str(part);
    }
    Some(path)
}

#[derive(Debug, Snafu)]
pub enum ReadError {
    #[snafu(display("no file {path} on the boot volume"))]
    NotFound { path: String },
    #[snafu(display("reading {path} from the boot volume failed"))]
    Read { path: String, source: uefi::Error },
}

/// All the bytes of the regular file at `path` (`\`-separated, from the root) on the boot volume.
pub fn read_file(path: &str) -> Result<Vec<u8>, ReadError> {
    let fail = |source: uefi::Error| {
        if source.status() == Status::NOT_FOUND {
            ReadError::NotFound { path: path.into() }
        } else {
            ReadError::Read { path: path.into(), source }
        }
    };
    let name = CString16::try_from(path).map_err(|_| fail(Status::INVALID_PARAMETER.into()))?;
    let mut volume = image_volume().map_err(fail)?;
    let mut root = volume.open_volume().map_err(fail)?;
    let handle = root.open(&name, FileMode::Read, FileAttribute::empty()).map_err(fail)?;
    let mut file = handle.into_regular_file().ok_or_else(|| fail(Status::UNSUPPORTED.into()))?;
    let info = file.get_boxed_info::<FileInfo>().map_err(fail)?;
    let mut bytes =
        alloc::vec![0_u8; usize::try_from(info.file_size()).map_err(|_| fail(Status::OUT_OF_RESOURCES.into()))?];
    let mut filled = 0;
    while filled < bytes.len() {
        match file.read(&mut bytes[filled..]).map_err(fail)? {
            0 => break,
            n => filled += n,
        }
    }
    bytes.truncate(filled);
    Ok(bytes)
}

/// The names of the regular files directly in the directory at `path` (`\`-separated, from the root)
/// on the boot volume.
pub fn list_files(path: &str) -> Result<Vec<String>, ReadError> {
    let fail = |source: uefi::Error| {
        if source.status() == Status::NOT_FOUND {
            ReadError::NotFound { path: path.into() }
        } else {
            ReadError::Read { path: path.into(), source }
        }
    };
    let name = CString16::try_from(path).map_err(|_| fail(Status::INVALID_PARAMETER.into()))?;
    let mut volume = image_volume().map_err(fail)?;
    let mut root = volume.open_volume().map_err(fail)?;
    let handle = root.open(&name, FileMode::Read, FileAttribute::empty()).map_err(fail)?;
    let mut dir = handle.into_directory().ok_or_else(|| fail(Status::UNSUPPORTED.into()))?;
    let mut names = Vec::new();
    while let Some(info) = dir.read_entry_boxed().map_err(fail)? {
        if !info.is_directory() {
            names.push(info.file_name().to_string());
        }
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use assert2::check;
    use rstest::rstest;

    use super::confined_path;

    #[rstest]
    #[case("regulatory.db", Some("\\EFI\\ferrite\\firmware\\regulatory.db"))]
    #[case("rtw89/rtw8852c_fw-2.bin", Some("\\EFI\\ferrite\\firmware\\rtw89\\rtw8852c_fw-2.bin"))]
    #[case("../loader.efi", None)]
    #[case("a/../b", None)]
    #[case("/etc/passwd", None)]
    #[case("a//b", None)]
    #[case("a\\b", None)]
    #[case("", None)]
    #[case("C:x", None)]
    fn firmware_names_stay_below_the_directory(#[case] name: &str, #[case] want: Option<&str>) {
        check!(confined_path("\\EFI\\ferrite\\firmware", name).as_deref() == want);
    }
}
