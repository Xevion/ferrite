//! Tripwires over the compiled Linux objects, which the UEFI port must keep true.
//!
//! The C is later linked into a UEFI image that has no dynamic loader and runs without the SSE state
//! being saved, so the objects may only use the relocation kinds a static link resolves and must not
//! touch vector registers. They also must not mask interrupts or trap on their own: the shadow headers
//! route those to shim calls.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use assert2::check;

const ARCHIVE: &str = env!("FERRITE_WIFI_ARCHIVE");
const DEFAULT_LLVM: &str = "/usr/lib/llvm-21/bin";
const COLLISIONS: &str = include_str!("../libc-collisions.txt");
const INITCALL_TABLE: &str = env!("FERRITE_WIFI_INITCALLS");
const ALLOWED_RELOCATIONS: [&str; 5] =
    ["R_X86_64_64", "R_X86_64_PC32", "R_X86_64_PLT32", "R_X86_64_GOTPCRELX", "R_X86_64_REX_GOTPCRELX"];

fn llvm(tool: &str, args: &[&str]) -> String {
    llvm_on(tool, args, ARCHIVE)
}

fn llvm_on(tool: &str, args: &[&str], file: &str) -> String {
    let dir = std::env::var_os("FERRITE_LLVM")
        .filter(|v| !v.is_empty())
        .map_or_else(|| PathBuf::from(DEFAULT_LLVM), PathBuf::from);
    let output = Command::new(dir.join(tool)).args(args).arg(file).output().expect("run the LLVM tool");
    assert!(output.status.success(), "{tool} failed: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).expect("LLVM tool output is UTF-8")
}

/// Relocations of the code and data. The debug sections the lab build adds carry relocations of their own
/// kinds, which the UEFI build does not have, so they are left out.
fn relocation_counts() -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    let mut in_debug_section = false;
    for line in llvm("llvm-readelf", &["-r"]).lines() {
        if let Some(section) = line.strip_prefix("Relocation section '") {
            in_debug_section = section.starts_with(".rela.debug_");
        } else if !in_debug_section
            && let Some(kind) = line.split_whitespace().nth(2).filter(|kind| kind.starts_with("R_"))
        {
            *counts.entry(kind.to_owned()).or_insert(0) += 1;
        }
    }
    counts
}

/// The disassembly of the whole archive, one instruction per line as `(function, mnemonic, operands)`.
fn instructions() -> &'static [(String, String, String)] {
    static CACHE: OnceLock<Vec<(String, String, String)>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let mut function = String::new();
        let mut found = Vec::new();
        for line in llvm("llvm-objdump", &["-d", "--no-show-raw-insn"]).lines() {
            if let Some(label) = line.strip_suffix(">:").and_then(|l| l.split_once(" <")) {
                label.1.clone_into(&mut function);
            } else if let Some((address, rest)) = line.split_once(':')
                && !address.trim().is_empty()
                && address.trim().bytes().all(|b| b.is_ascii_hexdigit())
            {
                let (mnemonic, operands) = rest.trim_start().split_once('\t').unwrap_or_else(|| (rest.trim(), ""));
                found.push((function.clone(), mnemonic.to_owned(), operands.to_owned()));
            }
        }
        found
    })
}

fn defined_symbols() -> BTreeSet<String> {
    llvm("llvm-nm", &["--defined-only", "--extern-only"])
        .lines()
        .filter_map(|l| l.split_whitespace().nth(2))
        .map(str::to_owned)
        .collect()
}

fn undefined_symbols() -> BTreeSet<String> {
    llvm("llvm-nm", &["--undefined-only"])
        .lines()
        .filter_map(|l| l.trim().strip_prefix("U "))
        .map(str::to_owned)
        .collect()
}

/// `(libc name, shim name)` pairs of `libc-collisions.txt`.
fn collisions() -> Vec<(&'static str, &'static str)> {
    COLLISIONS
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .filter_map(|line| line.split_once(' '))
        .map(|(libc, shim)| (libc.trim(), shim.trim()))
        .collect()
}

fn count(mnemonic: &str) -> usize {
    instructions().iter().filter(|(_, m, _)| m == mnemonic).count()
}

#[test]
fn only_the_relocation_kinds_a_static_link_resolves_appear() {
    let counts = relocation_counts();
    check!(!counts.is_empty(), "the relocation dump was not parsed");
    let others: Vec<_> = counts.iter().filter(|(kind, _)| !ALLOWED_RELOCATIONS.contains(&kind.as_str())).collect();
    check!(others.is_empty(), "unsupported relocations: {others:?}");
}

#[test]
fn no_vector_register_is_used() {
    check!(instructions().len() > 100_000, "the disassembly was not parsed");
    let uses_vector = |operands: &str| {
        let registers = operands.split('<').next().unwrap_or_default();
        ["%xmm", "%ymm", "%zmm"].iter().any(|prefix| registers.contains(prefix))
    };
    let offenders: Vec<_> = instructions()
        .iter()
        .filter(|(_, _, operands)| uses_vector(operands))
        .map(|(function, mnemonic, operands)| format!("{function}: {mnemonic} {operands}"))
        .take(5)
        .collect();
    check!(offenders.is_empty(), "vector registers in: {offenders:#?}");
}

#[test]
fn interrupts_are_never_masked_by_instruction() {
    check!(instructions().len() > 100_000, "the disassembly was not parsed");
    check!(count("cli") == 0);
    check!(count("sti") == 0);
}

#[test]
fn nothing_traps_and_no_bug_table_remains() {
    check!(instructions().len() > 100_000, "the disassembly was not parsed");
    check!(count("ud2") == 0, "BUG and WARN must call the shim, not trap");
    check!(count("ud1") == 0);
    check!(!llvm("llvm-readelf", &["-S"]).contains("__bug_table"));
}

#[test]
fn the_wireless_initcalls_and_exitcalls_are_all_present() {
    let in_archive = |prefix: &str| -> BTreeSet<String> {
        defined_symbols().into_iter().filter_map(|s| s.strip_prefix(prefix).map(str::to_owned)).collect()
    };
    let (inits, exits) = (in_archive("__lx_initcall_"), in_archive("__lx_exitcall_"));
    for call in ["4_ieee80211_init", "5_cfg80211_init", "6_rtw89_8852ce_driver_init", "7_regulatory_init_db"] {
        check!(inits.contains(call), "missing initcall {call}");
    }
    for call in ["ieee80211_exit", "cfg80211_exit", "rtw89_8852ce_driver_exit"] {
        check!(exits.contains(call), "missing exitcall {call}");
    }
}

#[test]
fn the_tables_list_initcalls_in_level_order_and_exitcalls_in_reverse() {
    let table = std::fs::read_to_string(INITCALL_TABLE).expect("read the generated initcall table");
    let entries = |section: &str| -> Vec<String> {
        let body = table.split_once(section).map_or("", |(_, rest)| rest).split("};").next().unwrap_or_default();
        body.lines().filter(|l| l.starts_with("\t{")).map(|l| l.trim().to_owned()).collect()
    };
    let field = |entry: &str, n: usize| entry.split('"').nth(n).unwrap_or_default().to_owned();
    let inits = entries("lx_initcalls[] = {");
    let rank = |level: &str| {
        ["early", "0", "1", "1s", "2", "2s", "3", "3s", "4", "4s", "5", "5s", "rootfs", "6", "6s", "7", "7s"]
            .iter()
            .position(|l| *l == level)
    };
    let levels: Vec<_> = inits.iter().map(|l| rank(&field(l, 1))).collect();
    check!(levels.iter().all(Option::is_some));
    check!(levels.is_sorted());
    let order = |entries: &[String], n: usize, names: &[&str]| -> Vec<usize> {
        names.iter().filter_map(|name| entries.iter().position(|e| field(e, n) == *name)).collect()
    };
    let wireless = ["ieee80211_init", "cfg80211_init", "rtw89_8852ce_driver_init", "regulatory_init_db"];
    check!(order(&inits, 3, &wireless).is_sorted());
    let exits = entries("lx_exitcalls[] = {");
    let exit_order = order(&exits, 1, &["rtw89_8852ce_driver_exit", "cfg80211_exit", "ieee80211_exit"]);
    check!(exit_order.len() == 3 && exit_order.is_sorted());
}

/// Memory functions the archive may import from libc by name, since the libc versions do the same job. It
/// never defines one: a definition would replace libc's for the whole lab process.
const SHARED_WITH_LIBC: [&str; 3] = ["memcpy", "memmove", "memset"];

/// The host libc: what the C compiler reports, else the usual Debian and Ubuntu locations.
fn host_libc() -> String {
    let reported = Command::new("cc").arg("-print-file-name=libc.so.6").output().ok().filter(|o| o.status.success());
    let from_cc = reported.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());
    let fixed = ["/lib/x86_64-linux-gnu/libc.so.6", "/usr/lib/x86_64-linux-gnu/libc.so.6", "/usr/lib64/libc.so.6"];
    from_cc
        .into_iter()
        .chain(fixed.map(str::to_owned))
        .find(|path| std::path::Path::new(path).is_absolute() && std::path::Path::new(path).exists())
        .expect("no host libc.so.6 found: `cc -print-file-name=libc.so.6` and the usual paths all failed")
}

/// The names libc exports dynamically, without their symbol versions.
fn libc_exports() -> BTreeSet<String> {
    llvm_on("llvm-nm", &["-D", "--defined-only"], &host_libc())
        .lines()
        .filter_map(|l| l.split_whitespace().nth(2))
        .map(|name| name.split('@').next().unwrap_or(name).to_owned())
        .collect()
}

#[test]
fn the_archive_shares_no_name_with_libc_beyond_the_memory_functions() {
    let exports = libc_exports();
    check!(exports.contains("malloc"), "the libc export list was not parsed");
    let defined = defined_symbols();
    let shared = |name: &String| SHARED_WITH_LIBC.contains(&name.as_str()) && !defined.contains(name);
    let clashes: Vec<_> =
        defined.union(&undefined_symbols()).filter(|name| exports.contains(*name) && !shared(name)).cloned().collect();
    check!(clashes.is_empty(), "names libc also exports, to add to libc-collisions.txt: {clashes:?}");
}

#[test]
fn renamed_libc_names_are_neither_imported_nor_defined() {
    let (defined, undefined) = (defined_symbols(), undefined_symbols());
    check!(!collisions().is_empty(), "libc-collisions.txt was not parsed");
    for (libc, shim) in collisions() {
        check!(!SHARED_WITH_LIBC.contains(&libc), "{libc} is renamed but also allowed to be shared");
        check!(!undefined.contains(libc), "{libc} is still imported");
        check!(!defined.contains(libc), "the archive defines {libc}");
        check!(defined.contains(shim) || undefined.contains(shim), "{shim} is neither defined nor imported");
    }
}

/// Prints the numbers the shadow tree is measured by; run with `--ignored --nocapture`.
#[test]
#[ignore = "baseline report only, asserts nothing"]
fn baseline() {
    println!("relocations: {:?}", relocation_counts());
    println!("cli {} sti {} ud2 {}", count("cli"), count("sti"), count("ud2"));
    let defined = defined_symbols();
    let missing = undefined_symbols().into_iter().filter(|s| !defined.contains(s)).count();
    println!("undefined and not defined in the archive: {missing}");
}
