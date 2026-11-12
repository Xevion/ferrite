//! The digest of everything the vendored snapshot is derived from.
//!
//! One definition, compiled into both `ferrite-dev vendor` (which writes it into the snapshot's VERSION
//! file) and this crate's `build.rs` (which refuses a snapshot whose digest differs).

use std::fs;
use std::io;
use std::path::Path;

use sha2::{Digest, Sha256};

/// Tracked files that decide what the snapshot holds, besides the patches.
const INPUT_FILES: [&str; 3] = ["vendor.toml", "source.list", "cflags.txt"];

/// Lower-case hex of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// The `.patch` files of `patches/` in file-name order; none when the directory is missing.
pub fn patch_files(crate_dir: &Path) -> io::Result<Vec<std::path::PathBuf>> {
    let entries = match fs::read_dir(crate_dir.join("patches")) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut patches = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "patch") {
            patches.push(path);
        }
    }
    patches.sort();
    Ok(patches)
}

/// Digest of the tracked inputs in `crate_dir`: each file's name and bytes, length-framed.
pub fn digest(crate_dir: &Path) -> io::Result<String> {
    let mut hasher = Sha256::new();
    let mut add = |name: &str, bytes: &[u8]| {
        hasher.update((name.len() as u64).to_le_bytes());
        hasher.update(name.as_bytes());
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    };
    for name in INPUT_FILES {
        add(name, &fs::read(crate_dir.join(name))?);
    }
    for patch in patch_files(crate_dir)? {
        let name = patch.file_name().map_or_default(|n| n.to_string_lossy().into_owned());
        add(&format!("patches/{name}"), &fs::read(&patch)?);
    }
    Ok(hex(&hasher.finalize()))
}

/// The line of VERSION that carries the digest.
pub fn version_line(digest: &str) -> String {
    format!("inputs {digest}")
}
