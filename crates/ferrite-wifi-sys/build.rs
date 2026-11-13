//! Compiles the Linux C files of `source.list` from the `linux/` snapshot into one static archive.
//!
//! The snapshot is made by `ferrite-dev vendor`. Every flag is passed explicitly: `cc`'s own defaults
//! (profile optimisation level, PIC, target flags) would change what the kernel headers assume.
//!
//! C files under `shadow/` whose path is a Linux path (`shadow/kernel/sched/core.c`) replace that Linux file:
//! they are compiled as Linux units with the KBUILD defines of the path they shadow, and a shadowed path must
//! not also be in `source.list`.
//!
//! Hand-written and generated C in `glue/` (the kernel symbols the shim defines in C) is compiled with the
//! same flags into the same archive, after the passes below and so without the rename.
//!
//! On a Linux target the C is also built with unwind tables and debug info, for the lab's backtraces.
//!
//! Two passes run over each object before archiving: the symbols of `libc-collisions.txt` are renamed so
//! they cannot bind to the host libc, and the initcall and exitcall pointers the shadow `linux/init.h`
//! emits are collected into ordered tables (`lx_initcalls`, `lx_exitcalls`) compiled into the archive.

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, exit};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

mod vendor_inputs;

const DEFAULT_LLVM: &str = "/usr/lib/llvm-21/bin";
const ARCHIVE: &str = "ferrite_wifi_c";

/// Objtool annotation sections, which the kernel's linker script discards; their relocations would not link.
const DISCARD: &str = "--remove-section=.discard.*";

/// Initcall levels in the order the kernel runs them. `console_initcall` is not among them.
const LEVELS: [&str; 17] =
    ["early", "0", "1", "1s", "2", "2s", "3", "3s", "4", "4s", "5", "5s", "rootfs", "6", "6s", "7", "7s"];
const INITCALL_PREFIX: &str = "__lx_initcall_";
const EXITCALL_PREFIX: &str = "__lx_exitcall_";

/// What the lab, the Linux process this C runs in, adds so a backtrace can walk through C frames and name
/// them. The target OS selects it: the lab is the only Linux build, and the UEFI build has no use for it.
const LAB_FLAGS: [&str; 3] = ["-g", "-fno-omit-frame-pointer", "-fasynchronous-unwind-tables"];

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

/// The KBUILD defines Kbuild gives a built-in object at `path`, a Linux path with its extension.
fn kbuild_defines(path: &str) -> Vec<String> {
    let (dir, name) = path.rsplit_once('/').unwrap_or((".", path));
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    vec![
        format!("-DKBUILD_BASENAME=\"{stem}\""),
        format!("-DKBUILD_MODNAME=\"{stem}\""),
        format!("-D__KBUILD_MODNAME=kmod_{stem}"),
        format!("-DKBUILD_MODFILE=\"{dir}/{stem}\""),
    ]
}

/// The `.c` files under `dir`, as paths relative to it with `/` separators, in name order.
fn shadow_sources(dir: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, found: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        for path in entries.filter_map(Result::ok).map(|entry| entry.path()) {
            if path.is_dir() {
                walk(root, &path, found);
            } else if path.extension().is_some_and(|ext| ext == "c")
                && let Ok(relative) = path.strip_prefix(root)
            {
                let parts: Vec<String> =
                    relative.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
                found.push(parts.join("/"));
            }
        }
    }
    let mut found = Vec::new();
    walk(dir, dir, &mut found);
    found.sort();
    found
}

/// One C file compiled as a Linux unit: `path` is the Linux path it stands for, `file` where it is read from.
struct Unit {
    path: String,
    file: String,
    kbuild: Vec<String>,
}

/// Every C file to compile as a Linux unit: `source.list` from the snapshot, then the shadows.
fn units(crate_dir: &Path, snapshot: &Path) -> Vec<Unit> {
    let listed = source_list(&read(&crate_dir.join("source.list")));
    let flags_text = read(&snapshot.join("kbuild-flags.txt"));
    let per_file = file_flags(&flags_text);
    let shadowed = shadow_sources(&crate_dir.join("shadow"));
    let both: Vec<&str> = shadowed.iter().filter(|path| listed.contains(path)).map(String::as_str).collect();
    if !both.is_empty() {
        fail(&format!("shadowed files are also in source.list: {}", both.join(", ")));
    }
    let mut units: Vec<Unit> = listed
        .iter()
        .map(|source| {
            let Some(kbuild) = per_file.get(source.as_str()) else {
                fail(&format!("{source} is in source.list but not in the snapshot; run `just vendor`"));
            };
            let kbuild = kbuild.iter().map(|flag| (*flag).to_owned()).collect();
            Unit { path: source.clone(), file: format!("linux/{source}"), kbuild }
        })
        .collect();
    units.extend(shadowed.into_iter().map(|path| Unit {
        file: format!("shadow/{path}"),
        kbuild: kbuild_defines(&path),
        path,
    }));
    units
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

/// Stdout of an LLVM tool; any failure ends the build.
fn run_llvm(llvm: &Path, tool: &str, args: &[&OsStr]) -> String {
    let output = Command::new(llvm.join(tool))
        .args(args)
        .output()
        .unwrap_or_else(|e| fail(&format!("cannot run {tool}: {e}; set FERRITE_LLVM to the LLVM bin directory")));
    if !output.status.success() {
        fail(&format!("{tool} failed: {}", String::from_utf8_lossy(&output.stderr)));
    }
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The external symbols `object` defines.
fn defined_symbols(llvm: &Path, object: &Path) -> Vec<String> {
    let args = ["--defined-only".as_ref(), "--extern-only".as_ref(), object.as_os_str()];
    run_llvm(llvm, "llvm-nm", &args).lines().filter_map(|l| l.split_whitespace().nth(2)).map(str::to_owned).collect()
}

/// One entry of an initcall or exitcall table, found in object number `slot`.
struct Call {
    slot: usize,
    level: &'static str,
    name: String,
    symbol: String,
}

/// The initcall and exitcall pointers among `symbols`, in symbol order.
fn calls(slot: usize, symbols: &[String]) -> (Vec<Call>, Vec<Call>) {
    let (mut inits, mut exits) = (Vec::new(), Vec::new());
    for symbol in symbols {
        if let Some(rest) = symbol.strip_prefix(INITCALL_PREFIX) {
            let found =
                rest.split_once('_').and_then(|(level, name)| Some((LEVELS.iter().find(|l| **l == level)?, name)));
            let Some((level, name)) = found else {
                fail(&format!("{symbol} is not an initcall of a known level"));
            };
            inits.push(Call { slot, level, name: name.to_owned(), symbol: symbol.clone() });
        } else if let Some(name) = symbol.strip_prefix(EXITCALL_PREFIX) {
            exits.push(Call { slot, level: "", name: name.to_owned(), symbol: symbol.clone() });
        }
    }
    (inits, exits)
}

/// Calls whose pointer symbol more than one object defines, as `symbol: source, source`. An archive link
/// takes the first and may never pull the other member, so one module's call would be lost without a word.
fn duplicates(calls: &[Call], sources: &[String]) -> Vec<String> {
    let mut by_symbol: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for call in calls {
        by_symbol.entry(&call.symbol).or_default().push(sources.get(call.slot).map_or("?", String::as_str));
    }
    by_symbol
        .into_iter()
        .filter(|(_, owners)| owners.len() > 1)
        .map(|(symbol, owners)| format!("{symbol}: {}", owners.join(", ")))
        .collect()
}

/// The tables as C. Initcalls run in level order, and within a level in link order. Exitcalls run in the
/// reverse of their object's first initcall, so a module is torn down before what it was set up after;
/// an object with no initcall is torn down last.
fn call_tables(mut inits: Vec<Call>, mut exits: Vec<Call>) -> String {
    let rank = |level: &str| LEVELS.iter().position(|l| *l == level);
    inits.sort_by_key(|call| (rank(call.level), call.slot));
    let first_init = |slot: usize| inits.iter().position(|call| call.slot == slot);
    exits.sort_by_key(|call| std::cmp::Reverse(first_init(call.slot)));

    let lines = |calls: &[Call], line: &dyn Fn(&Call) -> String| -> String {
        calls.iter().map(|call| line(call) + "\n").collect()
    };
    let externs = lines(&inits, &|c| format!("extern int (*{})(void);", c.symbol))
        + &lines(&exits, &|c| format!("extern void (*{})(void);", c.symbol));
    let init_rows = lines(&inits, &|c| format!("\t{{ \"{}\", \"{}\", &{} }},", c.level, c.name, c.symbol));
    let exit_rows = lines(&exits, &|c| format!("\t{{ \"{}\", &{} }},", c.name, c.symbol));
    format!(
        "/* Generated by build.rs from the __lx_initcall_* and __lx_exitcall_* symbols. */\n\
         struct lx_initcall {{ const char *level; const char *name; int (*const *slot)(void); }};\n\
         struct lx_exitcall {{ const char *name; void (*const *slot)(void); }};\n\n\
         {externs}\n\
         const struct lx_initcall lx_initcalls[] = {{\n{init_rows}}};\n\
         const unsigned long lx_initcall_count = {};\n\n\
         const struct lx_exitcall lx_exitcalls[] = {{\n{exit_rows}}};\n\
         const unsigned long lx_exitcall_count = {};\n",
        inits.len(),
        exits.len(),
    )
}

/// The C of the call tables over every compiled object, refusing a call that two files define.
fn checked_call_tables(compiled: &[(usize, PathBuf, Vec<String>)], sources: &[String]) -> String {
    let (mut inits, mut exits) = (Vec::new(), Vec::new());
    for (slot, _, symbols) in compiled {
        let (init, exit) = calls(*slot, symbols);
        inits.extend(init);
        exits.extend(exit);
    }
    let clashes: Vec<String> = duplicates(&inits, sources).into_iter().chain(duplicates(&exits, sources)).collect();
    if !clashes.is_empty() {
        fail(&format!("initcalls or exitcalls of the same name in several files: {}", clashes.join("; ")));
    }
    call_tables(inits, exits)
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

/// The `.c` files of `glue/`, in name order; none when the directory is missing.
fn glue_sources(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    let mut sources: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "c"))
        .collect();
    sources.sort();
    sources
}

/// Compile each glue file as a unit of a module named `glue`; headers outside the include roots are
/// reachable as `"linux/..."` from the crate directory.
fn glue_objects(crate_dir: &Path, clang: &Path, llvm: &Path, flags: &[String]) -> Vec<PathBuf> {
    glue_sources(&crate_dir.join("glue"))
        .into_iter()
        .flat_map(|source| {
            let stem = source.file_stem().map_or_else(String::new, |s| s.to_string_lossy().into_owned());
            let defines = [
                format!("-DKBUILD_BASENAME=\"{stem}\""),
                "-DKBUILD_MODNAME=\"glue\"".to_owned(),
                "-D__KBUILD_MODNAME=kmod_glue".to_owned(),
                format!("-DKBUILD_MODFILE=\"glue/{stem}\""),
                "-I.".to_owned(),
            ];
            let extra: Vec<&str> = defines.iter().map(String::as_str).collect();
            let mut build = builder(clang, llvm, flags, &extra);
            build.file(&source);
            build.compile_intermediates()
        })
        .collect()
}

fn main() {
    let crate_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap_or_else(|| fail("CARGO_MANIFEST_DIR is not set")));
    println!("cargo:rerun-if-env-changed=FERRITE_LLVM");
    for tracked in [
        "build.rs",
        "vendor_inputs.rs",
        "vendor.toml",
        "cflags.txt",
        "source.list",
        "patches",
        "linux/VERSION",
        "shadow",
        "libc-collisions.txt",
        "glue",
    ] {
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

    let mut flags = global_flags(&read(&crate_dir.join("cflags.txt")));
    if env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "linux") {
        flags.extend(LAB_FLAGS.map(str::to_owned));
    }
    let units = units(&crate_dir, &snapshot);
    let sources: Vec<String> = units.iter().map(|unit| unit.path.clone()).collect();

    let collisions = crate_dir.join("libc-collisions.txt");
    let rename = format!("--redefine-syms={}", collisions.display());

    let next = AtomicUsize::new(0);
    let workers = thread::available_parallelism().map_or(1, usize::from).min(units.len().max(1));
    let compile = || {
        let mut done = Vec::new();
        loop {
            let slot = next.fetch_add(1, Ordering::Relaxed);
            let Some(unit) = units.get(slot) else { return done };
            let dir = unit.path.rsplit_once('/').map_or(".", |(dir, _)| dir);
            let (include, generated) = (format!("-Ilinux/{dir}"), format!("-Ilinux/generated/{dir}"));
            let mut extra: Vec<&str> = unit.kbuild.iter().map(String::as_str).collect();
            extra.extend([include.as_str(), generated.as_str()]);
            let mut build = builder(&clang, &llvm, &flags, &extra);
            build.file(&unit.file);
            let object = build
                .compile_intermediates()
                .pop()
                .unwrap_or_else(|| fail(&format!("{} produced no object", unit.file)));
            run_llvm(&llvm, "llvm-objcopy", &[rename.as_ref(), DISCARD.as_ref(), object.as_os_str()]);
            let symbols = defined_symbols(&llvm, &object);
            done.push((slot, object, symbols));
        }
    };
    let mut compiled: Vec<(usize, PathBuf, Vec<String>)> = thread::scope(|scope| {
        #[expect(clippy::needless_collect, reason = "every thread must start before the first join")]
        let handles: Vec<_> = (0..workers).map(|_| scope.spawn(compile)).collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap_or_else(|_| fail("a compile thread panicked")))
            .collect()
    });
    compiled.sort_by_key(|(slot, ..)| *slot);
    let tables = checked_call_tables(&compiled, &sources);
    let mut objects: Vec<PathBuf> = compiled.into_iter().map(|(_, object, _)| object).collect();

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap_or_else(|| fail("OUT_DIR is not set")));
    let table = out_dir.join("initcalls.c");
    fs::write(&table, tables).unwrap_or_else(|e| fail(&format!("cannot write {}: {e}", table.display())));
    let mut build = builder(&clang, &llvm, &flags, &[]);
    build.file(&table);
    objects.extend(build.compile_intermediates());
    println!("cargo:rustc-env=FERRITE_WIFI_INITCALLS={}", table.display());
    objects.extend(glue_objects(&crate_dir, &clang, &llvm, &flags));

    let archive = builder(&clang, &llvm, &[], &[]).create_archive(ARCHIVE, &objects);
    println!("cargo:rustc-env=FERRITE_WIFI_ARCHIVE={}", archive.display());
    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static={ARCHIVE}");
}
