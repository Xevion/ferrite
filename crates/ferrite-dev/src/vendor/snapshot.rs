//! The trimmed snapshot: the listed `.c` files, every header they include, and what the build needs to
//! know about them.
//!
//! Headers are found by asking clang which files each source reads, so the snapshot holds exactly what
//! the build can open.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;

use snafu::ResultExt;

use super::kbuild::{KbuildFile, kbuild_defines};
use super::kernel::Tree;
use super::{HeaderSnafu, IoSnafu, VendorError, exec};

/// The name of the snapshot's top directory inside the crate.
pub const SNAPSHOT_DIR: &str = "linux";
/// Written last, so its presence means the snapshot is complete.
pub const VERSION_FILE: &str = "VERSION";
/// Per-file compile arguments, one tab-separated line per source.
pub const FLAGS_FILE: &str = "kbuild-flags.txt";
/// Where the output tree sits inside the snapshot.
pub const GENERATED_DIR: &str = "generated";

/// A source and the extra arguments to compile it with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    pub source: String,
    pub flags: Vec<String>,
}

/// Units for every listed file: Kbuild's own flags where Kbuild builds it, else those of a standalone object.
pub fn units(listed: &[String], kbuild: &[KbuildFile]) -> Vec<Unit> {
    listed
        .iter()
        .map(|source| {
            let flags = kbuild.iter().find(|f| &f.source == source).map_or_else(
                || {
                    let (dir, name) = source.rsplit_once('/').unwrap_or((".", source));
                    let stem = name.trim_end_matches(".c");
                    kbuild_defines(dir, stem, stem)
                },
                |f| f.flags.clone(),
            );
            Unit { source: source.clone(), flags }
        })
        .collect()
}

/// A tree shaped like the snapshot, made of links into the source and output trees, so the paths clang
/// reports are the snapshot's own.
pub fn link_view(view: &Path, tree: &Tree) -> Result<(), VendorError> {
    let top = view.join(SNAPSHOT_DIR);
    fs::create_dir_all(&top).context(IoSnafu { what: "create", path: &top })?;
    let entries = fs::read_dir(&tree.src).context(IoSnafu { what: "list", path: &tree.src })?;
    for entry in entries {
        let entry = entry.context(IoSnafu { what: "list", path: &tree.src })?;
        let link = top.join(entry.file_name());
        std::os::unix::fs::symlink(entry.path(), &link).context(IoSnafu { what: "link", path: &link })?;
    }
    let generated = top.join(GENERATED_DIR);
    std::os::unix::fs::symlink(&tree.obj, &generated).context(IoSnafu { what: "link", path: &generated })
}

/// Resolve `.` and `..` textually. A header reached as `linux/net/mac80211/../x.h` is stored as `linux/net/x.h`.
fn normalize(path: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {},
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            },
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(out)
}

/// The files named in a `-MM` dependency rule, without the target.
fn parse_deps(rule: &str) -> Vec<&str> {
    let flat = rule.trim();
    let deps = flat.split_once(':').map_or("", |(_, deps)| deps);
    deps.split_whitespace().filter(|token| *token != "\\").collect()
}

/// Every file `units` read, as paths below the snapshot's top directory.
pub fn closure(view: &Path, llvm: &Path, cflags: &[String], units: &[Unit]) -> Result<BTreeSet<PathBuf>, VendorError> {
    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel();
    let workers = thread::available_parallelism().map_or(1, usize::from).min(units.len().max(1));
    thread::scope(|scope| {
        for _ in 0..workers {
            let tx = tx.clone();
            let next = &next;
            scope.spawn(move || {
                while let Some(unit) = units.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let mut clang = Command::new(llvm.join("clang"));
                    clang.current_dir(view).args(cflags).args(&unit.flags);
                    clang.arg(format!("-I{SNAPSHOT_DIR}/{}", parent_of(&unit.source)));
                    clang.arg("-MM").arg(format!("{SNAPSHOT_DIR}/{}", unit.source));
                    if tx.send((unit, exec(&mut clang, &format!("scan the includes of {}", unit.source)))).is_err() {
                        return;
                    }
                }
            });
        }
    });
    drop(tx);
    let mut files = BTreeSet::new();
    for (unit, result) in rx {
        for dep in parse_deps(&result?) {
            let path = normalize(dep).filter(|p| p.starts_with(SNAPSHOT_DIR));
            files.insert(path.ok_or_else(|| HeaderSnafu { header: dep, file: unit.source.clone() }.build())?);
        }
    }
    Ok(files)
}

fn parent_of(source: &str) -> &str {
    source.rsplit_once('/').map_or(".", |(dir, _)| dir)
}

/// Write the snapshot into `dest` (which must not exist), copying `files` out of `view`.
pub fn write(
    dest: &Path,
    view: &Path,
    files: &BTreeSet<PathBuf>,
    units: &[Unit],
    version: &str,
) -> Result<(), VendorError> {
    let top = dest;
    let copy = |from: &Path, to: &Path| -> Result<(), VendorError> {
        let bytes = fs::read(from).context(IoSnafu { what: "read", path: from })?;
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent).context(IoSnafu { what: "create", path: parent })?;
        }
        fs::write(to, bytes).context(IoSnafu { what: "write", path: to })
    };
    for file in files {
        let relative = file.strip_prefix(SNAPSHOT_DIR).unwrap_or(file);
        copy(&view.join(file), &top.join(relative))?;
    }
    let config = Path::new(SNAPSHOT_DIR).join(GENERATED_DIR).join(".config");
    copy(&view.join(&config), &top.join(GENERATED_DIR).join(".config"))?;

    let flags: String = units
        .iter()
        .map(|unit| {
            let mut line = unit.source.clone();
            for flag in &unit.flags {
                line.push('\t');
                line.push_str(flag);
            }
            line.push('\n');
            line
        })
        .collect();
    let flags_path = top.join(FLAGS_FILE);
    fs::write(&flags_path, flags).context(IoSnafu { what: "write", path: &flags_path })?;
    let version_path = top.join(VERSION_FILE);
    fs::write(&version_path, version).context(IoSnafu { what: "write", path: &version_path })
}

/// Total size in bytes of the regular files below `dir`.
pub fn size(dir: &Path) -> u64 {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| match entry.metadata() {
            Ok(meta) if meta.is_dir() => size(&entry.path()),
            Ok(meta) => meta.len(),
            Err(_) => 0,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    #[test]
    fn dependency_rules_lose_their_target_and_continuations() {
        let rule = "main.o: linux/net/a.c \\\n  linux/include/b.h linux/include/c.h\n";
        check!(parse_deps(rule) == ["linux/net/a.c", "linux/include/b.h", "linux/include/c.h"]);
    }

    #[test]
    fn paths_are_resolved_without_touching_the_disk() {
        check!(normalize("linux/net/mac80211/../x.h") == Some(PathBuf::from("linux/net/x.h")));
        check!(normalize("linux/./a/b.h") == Some(PathBuf::from("linux/a/b.h")));
        check!(normalize("../x.h").is_none());
        check!(normalize("/usr/include/stdio.h").is_none());
    }

    #[test]
    fn files_outside_kbuild_get_the_flags_of_a_standalone_object() {
        let kbuild = [KbuildFile { source: "net/a/x.c".into(), flags: vec!["-DDEBUG".into()] }];
        let got = units(&["net/a/x.c".to_owned(), "lib/vsprintf.c".to_owned()], &kbuild);
        check!(got[0].flags == ["-DDEBUG"]);
        check!(got[1].flags.contains(&"-DKBUILD_MODFILE=\"lib/vsprintf\"".to_owned()));
    }
}
