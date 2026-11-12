//! Tripwires over the compiled Linux objects, which the UEFI port must keep true.
//!
//! The C is later linked into a UEFI image that has no dynamic loader and runs without the SSE state
//! being saved, so the objects may only use the relocation kinds a static link resolves and must not
//! touch vector registers.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use assert2::check;

const ARCHIVE: &str = env!("FERRITE_WIFI_ARCHIVE");
const DEFAULT_LLVM: &str = "/usr/lib/llvm-21/bin";
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

/// Prints the numbers step 5 starts from; run with `--ignored --nocapture`.
#[test]
#[ignore = "baseline report only, asserts nothing"]
fn baseline() {
    let count = |mnemonic: &str| instructions().iter().filter(|(_, m, _)| m == mnemonic).count();
    println!("relocations: {:?}", relocation_counts());
    println!("cli {} sti {} ud2 {}", count("cli"), count("sti"), count("ud2"));
    let undefined: std::collections::BTreeSet<_> = llvm("llvm-nm", &["--undefined-only"])
        .lines()
        .filter_map(|l| l.trim().strip_prefix("U "))
        .map(str::to_owned)
        .collect();
    let defined: std::collections::BTreeSet<_> = llvm("llvm-nm", &["--defined-only", "--extern-only"])
        .lines()
        .filter_map(|l| l.split_whitespace().nth(2))
        .map(str::to_owned)
        .collect();
    println!("undefined and not defined in the archive: {}", undefined.difference(&defined).count());
}
