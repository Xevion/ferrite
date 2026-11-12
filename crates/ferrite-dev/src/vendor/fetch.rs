//! The cache directory and the checked tarball in it.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};
use snafu::ResultExt;

use super::inputs;
use super::spec::Pin;
use super::{IoSnafu, VendorError, exec};

/// `$FERRITE_CACHE`, else `$XDG_CACHE_HOME/ferrite-uefi`, else `$HOME/.cache/ferrite-uefi`.
pub fn cache_dir(var: impl Fn(&str) -> Option<OsString>) -> Result<PathBuf, VendorError> {
    let set = |name: &str| var(name).filter(|value| !value.is_empty());
    if let Some(dir) = set("FERRITE_CACHE") {
        return Ok(PathBuf::from(dir));
    }
    let base = match set("XDG_CACHE_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(set("HOME").ok_or(VendorError::NoHome)?).join(".cache"),
    };
    Ok(base.join("ferrite-uefi"))
}

/// The tarball in `cache`, downloaded when missing or corrupt, and always checked against the pinned hash.
pub fn ensure_tarball(cache: &Path, pin: &Pin) -> Result<PathBuf, VendorError> {
    fs::create_dir_all(cache).context(IoSnafu { what: "create", path: cache })?;
    let tarball = cache.join(pin.tarball_name());
    if tarball.exists() {
        if sha256_file(&tarball)? == pin.sha256 {
            return Ok(tarball);
        }
        println!("vendor: {} does not match the pinned hash, downloading it again", tarball.display());
        fs::remove_file(&tarball).context(IoSnafu { what: "remove", path: &tarball })?;
    }
    println!("vendor: downloading {}", pin.url);
    let partial = tarball.with_extension("xz.part");
    let mut curl = Command::new("curl");
    curl.args(["--fail", "--location", "--silent", "--show-error", "--output"]).arg(&partial).arg(&pin.url);
    exec(&mut curl, "download the Linux tarball")?;
    let got = sha256_file(&partial)?;
    if got != pin.sha256 {
        fs::remove_file(&partial).context(IoSnafu { what: "remove", path: &partial })?;
        return Err(VendorError::Hash { file: pin.url.clone(), want: pin.sha256.clone(), got });
    }
    fs::rename(&partial, &tarball).context(IoSnafu { what: "rename", path: &partial })?;
    Ok(tarball)
}

fn sha256_file(path: &Path) -> Result<String, VendorError> {
    let mut file = File::open(path).context(IoSnafu { what: "open", path })?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1 << 20];
    loop {
        let n = file.read(&mut buffer).context(IoSnafu { what: "read", path })?;
        if n == 0 {
            return Ok(inputs::hex(&hasher.finalize()));
        }
        hasher.update(&buffer[..n]);
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::override_wins(Some("/c"), Some("/x"), Some("/h"), Some("/c"))]
    #[case::xdg(None, Some("/x"), Some("/h"), Some("/x/ferrite-uefi"))]
    #[case::home(None, None, Some("/h"), Some("/h/.cache/ferrite-uefi"))]
    #[case::empty_values_ignored(Some(""), Some(""), Some("/h"), Some("/h/.cache/ferrite-uefi"))]
    #[case::nothing(None, None, None, None)]
    fn cache_location(
        #[case] cache: Option<&str>,
        #[case] xdg: Option<&str>,
        #[case] home: Option<&str>,
        #[case] want: Option<&str>,
    ) {
        let var = |name: &str| match name {
            "FERRITE_CACHE" => cache.map(OsString::from),
            "XDG_CACHE_HOME" => xdg.map(OsString::from),
            "HOME" => home.map(OsString::from),
            _ => None,
        };
        check!(cache_dir(var).ok() == want.map(PathBuf::from));
    }
}
