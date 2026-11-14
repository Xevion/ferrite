//! The settings file on the boot volume, read once when an app starts.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use ferrite_config::{ConfigError, Loaded, PATH};

use crate::volume::{self, ReadError};

/// The settings in `\EFI\ferrite\ferrite.toml`. No file, an unreadable one or one with problems is
/// logged, once per problem with its line and key, and what it cannot supply stays unconfigured.
#[must_use]
pub fn load() -> Loaded {
    from_file(volume::read_file(PATH))
}

fn from_file(file: Result<Vec<u8>, ReadError>) -> Loaded {
    match file {
        Ok(bytes) => {
            let loaded = ferrite_config::load(&bytes);
            for error in &loaded.errors {
                log::warn!("config: {PATH}: {error}; {}", consequence(error));
            }
            loaded
        },
        Err(ReadError::NotFound { .. }) => {
            log::info!("config: no {PATH}, nothing configured");
            Loaded::default()
        },
        Err(e) => {
            log::warn!("config: {e}, nothing configured");
            Loaded::default()
        },
    }
}

fn consequence(error: &ConfigError) -> String {
    error
        .section()
        .map_or_else(|| String::from("nothing is configured from it"), |section| format!("[{section}] is not used"))
}

#[cfg(test)]
mod tests {
    use assert2::check;
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::missing(Err(ReadError::NotFound { path: "x".into() }), false, false)]
    #[case::unreadable(Err(ReadError::Read { path: "x".into(), source: uefi::Status::DEVICE_ERROR.into() }), false, false)]
    #[case::not_toml(Ok(b"[run".to_vec()), false, true)]
    #[case::one_bad_section(Ok(b"[run]\npasses = 0\n[notify]\nurl = \"http://h/t\"\n".to_vec()), true, true)]
    #[case::good(Ok(b"[notify]\nurl = \"http://h/t\"\n".to_vec()), true, false)]
    fn a_file_that_cannot_be_used_configures_nothing_and_the_rest_still_applies(
        #[case] file: Result<Vec<u8>, ReadError>,
        #[case] notify: bool,
        #[case] errors: bool,
    ) {
        let loaded = from_file(file);
        check!(loaded.notify.is_some() == notify);
        check!(loaded.errors.is_empty() != errors);
    }
}
