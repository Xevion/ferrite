//! Compiles the Linux C files of `source.list` from the `linux/` snapshot into one static archive.
//!
//! The snapshot is made by `ferrite-dev vendor`. Every flag is passed explicitly: `cc`'s own defaults
//! (profile optimisation level, PIC, target flags) would change what the kernel headers assume.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, exit};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

mod vendor_inputs;

const DEFAULT_LLVM: &str = "/usr/lib/llvm-21/bin";
const ARCHIVE: &str = "ferrite_wifi_c";

/// The C always targets bare `x86_64`: `x86_64-unknown-uefi` is LLP64 with the MS ABI, which the kernel
/// headers do not support.
const C_TARGET: &str = "x86_64-unknown-none";

fn fail(message: &str) -> ! {
    eprintln!("ferrite-wifi-sys: {message}");
    exit(1)
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| fail(&format!("cannot read {}: {e}", path.display())))
}

/// The arguments of `cflags.txt`: every token outside `#` comment lines.
fn global_flags(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .flat_map(str::split_whitespace)
        .map(str::to_owned)
        .collect()
}

/// `source.list`: one path per line, `#` comments allowed.
fn source_list(text: &str) -> Vec<String> {
    text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(str::to_owned).collect()
}

/// `kbuild-flags.txt`: a source path followed by its tab-separated arguments, per line.
fn file_flags(text: &str) -> BTreeMap<&str, Vec<&str>> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            Some((fields.next()?, fields.collect()))
        })
        .collect()
}

/// The major version `clang --version` reports.
fn clang_major(clang: &Path) -> u32 {
    let output = Command::new(clang).arg("--version").output().unwrap_or_else(|e| {
        fail(&format!("cannot run {}: {e}; set FERRITE_LLVM to the LLVM bin directory", clang.display()))
    });
    let text = String::from_utf8_lossy(&output.stdout);
    text.split_once("clang version ")
        .and_then(|(_, rest)| rest.split('.').next())
        .and_then(|major| major.trim().parse().ok())
        .unwrap_or_else(|| fail(&format!("cannot read the version from `{}`", text.lines().next().unwrap_or(""))))
}

/// `CONFIG_CLANG_VERSION` from the snapshot's `autoconf.h`, as its major version.
fn configured_major(autoconf: &Path) -> u32 {
    let version = read(autoconf)
        .lines()
        .find_map(|line| line.strip_prefix("#define CONFIG_CLANG_VERSION "))
        .and_then(|value| value.trim().parse::<u32>().ok());
    version.map_or_else(|| fail(&format!("{} has no CONFIG_CLANG_VERSION", autoconf.display())), |v| v / 10_000)
}

/// A `cc` build set up for the kernel's flags and nothing else; `extra` follows the global ones.
fn builder(clang: &Path, llvm: &Path, flags: &[String], extra: &[&str]) -> cc::Build {
    let mut build = cc::Build::new();
    build
        .compiler(clang)
        .archiver(llvm.join("llvm-ar"))
        .target(C_TARGET)
        .no_default_flags(true)
        .warnings(false)
        .inherit_rustflags(false)
        .inherit_trim_paths(false)
        .cargo_metadata(false)
        .cargo_warnings(false);
    for flag in flags {
        build.flag(flag);
    }
    for flag in extra {
        build.flag(flag);
    }
    build
}

fn main() {
    let crate_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap_or_else(|| fail("CARGO_MANIFEST_DIR is not set")));
    println!("cargo:rerun-if-env-changed=FERRITE_LLVM");
    for tracked in
        ["build.rs", "vendor_inputs.rs", "vendor.toml", "cflags.txt", "source.list", "patches", "linux/VERSION"]
    {
        println!("cargo:rerun-if-changed={tracked}");
    }

    let snapshot = crate_dir.join("linux");
    let Ok(version) = fs::read_to_string(snapshot.join("VERSION")) else {
        fail("the Linux snapshot is missing, run `just vendor`")
    };
    let digest =
        vendor_inputs::digest(&crate_dir).unwrap_or_else(|e| fail(&format!("cannot read the vendor inputs: {e}")));
    if !version.lines().any(|line| line == vendor_inputs::version_line(&digest)) {
        fail("the Linux snapshot is out of date, run `just vendor`");
    }
    let llvm = env::var_os("FERRITE_LLVM")
        .filter(|v| !v.is_empty())
        .map_or_else(|| PathBuf::from(DEFAULT_LLVM), PathBuf::from);
    let clang = llvm.join("clang");
    let (have, want) =
        (clang_major(&clang), configured_major(&snapshot.join("generated/include/generated/autoconf.h")));
    if have != want {
        fail(&format!(
            "{} is clang {have}, but the snapshot was configured for clang {want}; set FERRITE_LLVM or re-run `just vendor`",
            clang.display()
        ));
    }

    let flags = global_flags(&read(&crate_dir.join("cflags.txt")));
    let sources = source_list(&read(&crate_dir.join("source.list")));
    let flags_text = read(&snapshot.join("kbuild-flags.txt"));
    let per_file = file_flags(&flags_text);

    let next = AtomicUsize::new(0);
    let workers = thread::available_parallelism().map_or(1, usize::from).min(sources.len().max(1));
    let compile = || {
        let mut done = Vec::new();
        loop {
            let slot = next.fetch_add(1, Ordering::Relaxed);
            let Some(source) = sources.get(slot) else { return done };
            let Some(kbuild) = per_file.get(source.as_str()) else {
                fail(&format!("{source} is in source.list but not in the snapshot; run `just vendor`"));
            };
            let dir = source.rsplit_once('/').map_or(".", |(dir, _)| dir);
            let include = format!("-Ilinux/{dir}");
            let mut extra: Vec<&str> = kbuild.clone();
            extra.push(&include);
            let mut build = builder(&clang, &llvm, &flags, &extra);
            build.file(format!("linux/{source}"));
            let object =
                build.compile_intermediates().pop().unwrap_or_else(|| fail(&format!("{source} produced no object")));
            done.push((slot, object));
        }
    };
    let mut compiled: Vec<(usize, PathBuf)> = thread::scope(|scope| {
        #[expect(clippy::needless_collect, reason = "every thread must start before the first join")]
        let handles: Vec<_> = (0..workers).map(|_| scope.spawn(compile)).collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap_or_else(|_| fail("a compile thread panicked")))
            .collect()
    });
    compiled.sort_by_key(|(slot, _)| *slot);
    let objects: Vec<PathBuf> = compiled.into_iter().map(|(_, object)| object).collect();

    let archive = builder(&clang, &llvm, &[], &[]).create_archive(ARCHIVE, &objects);
    println!("cargo:rustc-env=FERRITE_WIFI_ARCHIVE={}", archive.display());
}
