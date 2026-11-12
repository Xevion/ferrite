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
    let dir = std::env::var_os("FERRITE_LLVM")
        .filter(|v| !v.is_empty())
        .map_or_else(|| PathBuf::from(DEFAULT_LLVM), PathBuf::from);
    let output = Command::new(dir.join(tool)).args(args).arg(ARCHIVE).output().expect("run the LLVM tool");
    assert!(output.status.success(), "{tool} failed: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).expect("LLVM tool output is UTF-8")
}

fn relocation_counts() -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for kind in llvm("llvm-readelf", &["-r"]).lines().filter_map(|line| line.split_whitespace().nth(2)) {
        if kind.starts_with("R_") {
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
fn the_initcalls_are_exactly_the_four_expected() {
    let in_archive = |prefix: &str| -> BTreeSet<String> {
        defined_symbols().into_iter().filter_map(|s| s.strip_prefix(prefix).map(str::to_owned)).collect()
    };
    let strings = |names: &[&str]| names.iter().map(|n| (*n).to_owned()).collect::<BTreeSet<_>>();
    check!(
        in_archive("__lx_initcall_")
            == strings(&["4_ieee80211_init", "5_cfg80211_init", "6_rtw89_8852ce_driver_init", "7_regulatory_init_db"])
    );
    check!(in_archive("__lx_exitcall_") == strings(&["ieee80211_exit", "cfg80211_exit", "rtw89_8852ce_driver_exit"]));
}

#[test]
fn the_tables_list_initcalls_in_level_order_and_exitcalls_in_reverse() {
    let table = std::fs::read_to_string(INITCALL_TABLE).expect("read the generated initcall table");
    let entries = |section: &str| -> Vec<String> {
        let body = table.split_once(section).map_or("", |(_, rest)| rest).split("};").next().unwrap_or_default();
        body.lines().filter(|l| l.starts_with("\t{")).map(|l| l.trim().to_owned()).collect()
    };
    let inits = entries("lx_initcalls[] = {");
    let levels: Vec<_> = inits.iter().filter_map(|l| l.split('"').nth(1)).collect();
    check!(levels == ["4", "5", "6", "7"]);
    let exits = entries("lx_exitcalls[] = {");
    let names: Vec<_> = exits.iter().filter_map(|l| l.split('"').nth(1)).collect();
    check!(names == ["rtw89_8852ce_driver_exit", "cfg80211_exit", "ieee80211_exit"]);
}

#[test]
fn libc_colliding_names_are_renamed_and_never_defined() {
    let (defined, undefined) = (defined_symbols(), undefined_symbols());
    check!(collisions().len() == 5, "libc-collisions.txt was not parsed");
    for (libc, shim) in collisions() {
        check!(!undefined.contains(libc), "{libc} is still imported");
        check!(undefined.contains(shim), "{shim} is not imported");
        check!(!defined.contains(libc) && !defined.contains(shim), "the archive defines {libc} or {shim}");
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
