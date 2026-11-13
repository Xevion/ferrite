//! Firmware files read from `\EFI\ferrite\firmware\` on the volume the image was loaded from.

use alloc::format;
use alloc::string::ToString as _;
use alloc::vec::Vec;

use ferrite_efi::volume::{self, ReadError};
use ferrite_lkpi::firmware::{FirmwareError, FirmwareProvider};

/// Where firmware files sit, under their Linux names.
const FIRMWARE_DIR: &str = "\\EFI\\ferrite\\firmware";

pub struct VolumeFirmware;

impl FirmwareProvider for VolumeFirmware {
    fn load(&self, name: &str) -> Result<Vec<u8>, FirmwareError> {
        let Some(path) = volume::confined_path(FIRMWARE_DIR, name) else {
            log::warn!("refusing firmware name {name:?}: not a path below the firmware directory");
            return Err(FirmwareError::NotFound { name: name.to_string() });
        };
        match volume::read_file(&path) {
            Ok(bytes) => {
                log::debug!("firmware {name}: {} bytes", bytes.len());
                Ok(bytes)
            },
            Err(ReadError::NotFound { .. }) => Err(FirmwareError::NotFound { name: name.to_string() }),
            Err(ReadError::Read { source, .. }) => {
                Err(FirmwareError::Unreadable { name: name.to_string(), reason: format!("{:?}", source.status()) })
            },
        }
    }
}
