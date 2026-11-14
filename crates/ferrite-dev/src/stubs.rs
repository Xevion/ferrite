//! `stubs`: generate the kernel symbols the Wi-Fi C imports and nothing implements.
//!
//! The imports are what the archive of Linux objects leaves undefined, less what the shim crate and the
//! hand-written glue define. Each is classified from its declaration, found by asking clang to dump the
//! AST of one translation unit that includes every header the sources include. Functions become Rust
//! stubs in `ferrite-lkpi`; variables become zeroed C definitions and variadic functions C stubs, both in
//! the archive's `glue/` directory. Output is a pure function of the imports, so a rerun is a no-op.

pub(crate) mod artifacts;
mod ast;
mod classify;
mod include;
mod nm;
mod render;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use snafu::{OptionExt, ResultExt, Snafu, ensure};

use crate::vendor::spec::Pin;
use crate::vendor::spec::{Spec, parse_cflags, parse_source_list};
use crate::vendor::{VendorError, VendorOptions, fetch};
use artifacts::Artifacts;
use classify::{Class, Context, Import};
use include::Include;

const SHIM_CRATE: &str = "ferrite-lkpi";
const SYS_CRATE: &str = "ferrite-wifi-sys";
/// Where the generated Rust stubs go, relative to the shim crate.
const RUST_OUTPUT: &str = "src/stubs/generated.rs";
/// Where the generated C goes, relative to the Linux C crate.
const GLUE_DIR: &str = "glue";
const DATA_OUTPUT: &str = "generated_data.c";
const VARIADIC_OUTPUT: &str = "generated_variadic.c";
/// The build defines every Kbuild unit gets, which the headers read in a few places.
const UNIT_DEFINES: [&str; 4] = [
    "-DKBUILD_MODNAME=\"lkpi_stubs\"",
    "-DKBUILD_BASENAME=\"lkpi_stubs\"",
    "-D__KBUILD_MODNAME=kmod_lkpi_stubs",
    "-DKBUILD_MODFILE=\"lkpi_stubs\"",
];

#[derive(Debug, Snafu)]
pub enum StubsError {
    #[snafu(display("cannot {what} {}", path.display()))]
    Io { what: &'static str, path: PathBuf, source: io::Error },

    #[snafu(display("cannot run {program}; set FERRITE_LLVM to the LLVM bin directory if it is an LLVM tool"))]
    Spawn { program: String, source: io::Error },

    #[snafu(display("could not {what} ({status}):\n{detail}"))]
    Failed { what: String, status: String, detail: String },

    #[snafu(display("cannot read the build inputs"))]
    Inputs { source: VendorError },

    #[snafu(display("{what} is invalid: {reason}"))]
    Invalid { what: String, reason: String },

    #[snafu(display("cannot read {what}"))]
    Json { what: String, source: serde_json::Error },

    #[snafu(display("cannot find {what}"))]
    Missing { what: String },

    #[snafu(display("generated files are out of date, run `ferrite-dev stubs`: {}", files.join(", ")))]
    Drift { files: Vec<String> },
}

#[derive(Debug, Clone)]
pub struct StubsOptions {
    /// The `ferrite-wifi-sys` directory: its inputs, snapshot and `glue/`.
    pub wifi_dir: PathBuf,
    /// The `ferrite-lkpi` directory.
    pub lkpi_dir: PathBuf,
    /// Directory of the LLVM tools.
    pub llvm: PathBuf,
    /// Where `vendor` keeps the pinned Linux tarball, which the cut sources are read from.
    pub cache_dir: PathBuf,
    /// Report generated files that differ instead of writing them.
    pub check: bool,
}

impl StubsOptions {
    /// Options from the environment, which `vendor` reads too: `FERRITE_LLVM` and `FERRITE_CACHE`.
    pub fn from_env(wifi_dir: PathBuf, lkpi_dir: PathBuf, check: bool) -> Result<Self, StubsError> {
        let VendorOptions { llvm, cache_dir, .. } =
            VendorOptions::from_env(wifi_dir.clone(), false).context(InputsSnafu)?;
        let tidy = |dir: PathBuf| fs::canonicalize(&dir).unwrap_or(dir);
        Ok(Self { wifi_dir: tidy(wifi_dir), lkpi_dir: tidy(lkpi_dir), llvm, cache_dir, check })
    }
}

/// What a run found, and which generated files it changed.
#[derive(Debug)]
pub struct Report {
    pub total: usize,
    pub counts: [usize; Class::LABELS.len()],
    /// Imports no header declares, which get nothing generated.
    pub unknown: Vec<String>,
    pub changed: Vec<PathBuf>,
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let counts: Vec<String> =
            Class::LABELS.iter().zip(self.counts).map(|(label, count)| format!("{label} {count}")).collect();
        writeln!(f, "stubs: {} imports: {}", self.total, counts.join(", "))?;
        if !self.unknown.is_empty() {
            writeln!(f, "stubs: no declaration for {}", self.unknown.join(", "))?;
        }
        if self.changed.is_empty() {
            write!(f, "stubs: generated files unchanged")
        } else {
            let names: Vec<_> = self.changed.iter().map(|path| path.display().to_string()).collect();
            write!(f, "stubs: wrote {}", names.join(", "))
        }
    }
}

/// The tracked inputs of the Linux C build.
struct Inputs {
    flags: Vec<String>,
    roots: Vec<String>,
    sources: Vec<String>,
    /// Kbuild's own arguments for each source.
    unit_flags: BTreeMap<String, Vec<String>>,
    /// Shim name to the libc name it replaces.
    renames: BTreeMap<String, String>,
    pin: Pin,
    cut: Vec<String>,
}

fn read(path: &Path) -> Result<String, StubsError> {
    fs::read_to_string(path).context(IoSnafu { what: "read", path })
}

impl Inputs {
    fn load(wifi_dir: &Path) -> Result<Self, StubsError> {
        let flags = parse_cflags(&read(&wifi_dir.join("cflags.txt"))?);
        let sources = parse_source_list(&read(&wifi_dir.join("source.list"))?).context(InputsSnafu)?;
        let spec = Spec::parse(&read(&wifi_dir.join("vendor.toml"))?).context(InputsSnafu)?;
        let unit_flags = parse_unit_flags(&read(&wifi_dir.join("linux/kbuild-flags.txt"))?);
        let renames = parse_renames(&read(&wifi_dir.join("libc-collisions.txt"))?);
        Ok(Self {
            roots: include::include_roots(&flags),
            flags,
            sources,
            unit_flags,
            renames,
            pin: spec.pin,
            cut: spec.kbuild.cut,
        })
    }
}

/// `kbuild-flags.txt`: a source path, then its arguments, tab-separated.
fn parse_unit_flags(text: &str) -> BTreeMap<String, Vec<String>> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            Some((fields.next()?.to_owned(), fields.map(str::to_owned).collect()))
        })
        .collect()
}

/// `libc-collisions.txt`: `libc-name shim-name` per line; the result maps the shim name back.
fn parse_renames(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .filter_map(|line| line.split_once(' '))
        .map(|(libc, shim)| (shim.trim().to_owned(), libc.trim().to_owned()))
        .collect()
}

fn spawn_error(command: &Command) -> impl FnOnce(io::Error) -> StubsError {
    let program = command.get_program().to_string_lossy().into_owned();
    move |source| StubsError::Spawn { program, source }
}

fn failure(output: &Output, what: &str) -> StubsError {
    let text = String::from_utf8_lossy(&output.stderr);
    let lines: Vec<&str> = text.lines().collect();
    let detail = lines[lines.len().saturating_sub(20)..].join("\n");
    StubsError::Failed { what: what.to_owned(), status: output.status.to_string(), detail }
}

/// Run `command` to completion, and its output when it succeeds, else the tail of what it printed.
fn run_tool(command: &mut Command, what: &str) -> Result<Output, StubsError> {
    let output = command.output().map_err(spawn_error(command))?;
    if output.status.success() { Ok(output) } else { Err(failure(&output, what)) }
}

/// Run `command` with `input` on its standard input, whatever its exit status.
fn pipe(command: &mut Command, input: &[u8], what: &'static str) -> Result<Output, StubsError> {
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().map_err(spawn_error(command))?;
    let mut stdin = child.stdin.take().context(MissingSnafu { what: "the child's standard input" })?;
    match stdin.write_all(input) {
        Err(e) if e.kind() != io::ErrorKind::BrokenPipe => return Err(e).context(IoSnafu { what, path: "the child" }),
        _ => drop(stdin),
    }
    child.wait_with_output().context(IoSnafu { what, path: "the child" })
}

/// `work` over `items` on every core, results in the order of `items`.
fn parallel<T: Sync, R: Send>(items: &[T], work: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let workers = thread::available_parallelism().map_or(1, usize::from).min(items.len().max(1));
    let mut done: Vec<(usize, R)> = thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut mine = Vec::new();
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(item) = items.get(index) else { break };
                        mine.push((index, work(item)));
                    }
                    mine
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic)))
            .collect()
    });
    done.sort_by_key(|(index, _)| *index);
    done.into_iter().map(|(_, result)| result).collect()
}

/// Build the shim without its stubs and the Linux archive, and find them.
fn build(options: &StubsOptions) -> Result<Artifacts, StubsError> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = Command::new(cargo);
    command.current_dir(&options.wifi_dir).args(["build", "--message-format=json-render-diagnostics"]);
    command.args(["--no-default-features", "-p", SHIM_CRATE, "-p", SYS_CRATE]);
    let output = run_tool(&mut command, &format!("build {SHIM_CRATE} and {SYS_CRATE}"))?;
    artifacts::parse(&String::from_utf8_lossy(&output.stdout), SHIM_CRATE, SYS_CRATE)
}

/// The symbols the archive imports that neither the shim crate nor the hand-written glue defines.
fn missing_imports(options: &StubsOptions, artifacts: &Artifacts) -> Result<BTreeSet<String>, StubsError> {
    let nm = options.llvm.join("llvm-nm");
    let archive = run_tool(Command::new(&nm).arg(&artifacts.archive), "list the symbols of the Linux archive")?;
    let archive = nm::parse(&String::from_utf8_lossy(&archive.stdout), nm::is_generated_member);
    let mut command = Command::new(&nm);
    command.args(["--defined-only", "--extern-only"]).arg(&artifacts.rlib);
    let shim = run_tool(&mut command, "list the symbols of the shim crate")?;
    let shim = nm::parse(&String::from_utf8_lossy(&shim.stdout), |_| false);
    Ok(archive
        .undefined
        .difference(&archive.defined)
        .filter(|symbol| !shim.defined.contains(*symbol))
        .cloned()
        .collect())
}

/// Every header the sources include, as one list in an order that parses.
fn header_union(options: &StubsOptions, inputs: &Inputs) -> Result<Vec<Include>, StubsError> {
    let scan = |source: &String| -> Result<Vec<String>, StubsError> {
        let unit =
            inputs.unit_flags.get(source).context(MissingSnafu { what: format!("Kbuild flags for {source}") })?;
        let dir = source.rsplit_once('/').map_or(".", |(dir, _)| dir);
        let mut clang = Command::new(options.llvm.join("clang"));
        clang.current_dir(&options.wifi_dir).args(&inputs.flags).args(unit).arg(format!("-Ilinux/{dir}"));
        clang.arg(format!("-Ilinux/generated/{dir}"));
        clang.args(["-E", "-H", "-o", "/dev/null"]).arg(format!("linux/{source}"));
        let output = run_tool(&mut clang, &format!("list the includes of {source}"))?;
        Ok(include::direct_includes(&String::from_utf8_lossy(&output.stderr)).into_iter().map(str::to_owned).collect())
    };
    let per_source: Vec<Vec<String>> = parallel(&inputs.sources, scan).into_iter().collect::<Result<_, _>>()?;
    Ok(include::union_in_order(
        per_source.iter().map(|headers| headers.iter().map(String::as_str).collect()),
        &inputs.roots,
    ))
}

/// The declaration of `name` that clang finds when it parses `umbrella`.
fn declaration(
    options: &StubsOptions,
    inputs: &Inputs,
    umbrella: &str,
    name: &str,
) -> Result<Option<ast::Decl>, StubsError> {
    let mut clang = Command::new(options.llvm.join("clang"));
    clang.current_dir(&options.wifi_dir).args(&inputs.flags).args(UNIT_DEFINES);
    clang.args(["-I.", "-w", "-ferror-limit=0", "-fsyntax-only", "-x", "c", "-"]);
    clang.args(["-Xclang", "-ast-dump=json", "-Xclang"]).arg(format!("-ast-dump-filter={name}"));
    // clang rejects a few of the headers parsed out of context; the rest of the dump is still complete.
    let output = pipe(&mut clang, umbrella.as_bytes(), "read the AST dump of")?;
    ast::first_declaration(name, &output.stdout).context(JsonSnafu { what: format!("the AST dump for {name}") })
}

/// The extern symbols each cut source defines, mapped to that source.
///
/// The cut sources are not in the snapshot, so each is read out of the pinned tarball and compiled
/// with the flags of a sibling unit in its directory, against the snapshot's headers.
fn cut_definitions(options: &StubsOptions, inputs: &Inputs) -> Result<BTreeMap<String, String>, StubsError> {
    let mut defined = BTreeMap::new();
    if inputs.cut.is_empty() {
        return Ok(defined);
    }
    let tarball = fetch::ensure_tarball(&options.cache_dir, &inputs.pin).context(InputsSnafu)?;
    let root = inputs.pin.tarball_name().trim_end_matches(".tar.xz");
    for cut in &inputs.cut {
        let (dir, file) = cut.rsplit_once('/').unwrap_or((".", cut));
        let sibling = inputs.sources.iter().find(|source| source.rsplit_once('/').is_some_and(|(d, _)| d == dir));
        let unit = sibling
            .and_then(|source| inputs.unit_flags.get(source))
            .context(MissingSnafu { what: format!("a compiled source beside {cut}, whose build flags it shares") })?;
        let stem = file.trim_end_matches(".c");

        let mut tar = Command::new("tar");
        tar.arg("-xJf").arg(&tarball).args(["-O", &format!("{root}/{cut}")]);
        let source = run_tool(&mut tar, &format!("extract {cut} from the Linux tarball"))?;

        let mut clang = Command::new(options.llvm.join("clang"));
        clang.current_dir(&options.wifi_dir).args(&inputs.flags);
        clang.args(unit.iter().filter(|flag| !flag.starts_with("-DKBUILD_BASENAME=")));
        clang.arg(format!("-DKBUILD_BASENAME=\"{stem}\"")).arg(format!("-Ilinux/{dir}"));
        clang.args(["-w", "-c", "-x", "c", "-", "-o", "-"]);
        let object = pipe(&mut clang, &source.stdout, "compile a cut source")?;
        if !object.status.success() {
            return Err(failure(&object, &format!("compile {cut}")));
        }

        let mut nm = Command::new(options.llvm.join("llvm-nm"));
        nm.args(["--defined-only", "--extern-only", "-"]);
        let listing = pipe(&mut nm, &object.stdout, "list the symbols of a cut source")?;
        if !listing.status.success() {
            return Err(failure(&listing, &format!("list the symbols of {cut}")));
        }
        for symbol in nm::parse(&String::from_utf8_lossy(&listing.stdout), |_| false).defined {
            defined.insert(symbol, cut.clone());
        }
    }
    Ok(defined)
}

/// Classify every import.
fn classify_all(
    options: &StubsOptions,
    inputs: &Inputs,
    imports: &BTreeSet<String>,
) -> Result<(Vec<Import>, Vec<Include>), StubsError> {
    let seams = cut_definitions(options, inputs)?;
    let headers = header_union(options, inputs)?;
    let umbrella: String = headers.iter().map(|header| header.directive() + "\n").collect();
    let ctx = Context { roots: &inputs.roots, renames: &inputs.renames, seams: &seams };
    let symbols: Vec<&String> = imports.iter().collect();
    let found = parallel(&symbols, |symbol| declaration(options, inputs, &umbrella, ctx.declared_name(symbol)));
    let imports = symbols
        .into_iter()
        .zip(found)
        .map(|(symbol, decl)| Ok(classify::classify(symbol, decl?.as_ref(), &ctx)))
        .collect::<Result<_, StubsError>>()?;
    Ok((imports, headers))
}

/// Write each file whose content differs, or with `check` fail on the first difference instead.
fn publish(files: &[(PathBuf, String)], check: bool) -> Result<Vec<PathBuf>, StubsError> {
    let stale: Vec<&(PathBuf, String)> =
        files.iter().filter(|(path, content)| fs::read_to_string(path).ok().as_deref() != Some(content)).collect();
    ensure!(
        !check || stale.is_empty(),
        DriftSnafu { files: stale.iter().map(|(path, _)| path.display().to_string()).collect::<Vec<_>>() }
    );
    for (path, content) in &stale {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).context(IoSnafu { what: "create", path: dir })?;
        }
        fs::write(path, content).context(IoSnafu { what: "write", path })?;
    }
    Ok(stale.into_iter().map(|(path, _)| path.clone()).collect())
}

/// Blank the generated C that a previous run left, so that a build broken by it can be retried.
fn blank_generated_glue(options: &StubsOptions) -> Result<bool, StubsError> {
    let mut blanked = false;
    for name in [DATA_OUTPUT, VARIADIC_OUTPUT] {
        let path = options.wifi_dir.join(GLUE_DIR).join(name);
        if path.exists() {
            fs::write(&path, "/* Blanked by `ferrite-dev stubs`, which writes it again. */\n")
                .context(IoSnafu { what: "write", path })?;
            blanked = true;
        }
    }
    Ok(blanked)
}

pub fn run(options: &StubsOptions) -> Result<Report, StubsError> {
    let inputs = Inputs::load(&options.wifi_dir)?;
    // The archive includes the generated C, which a stale run can leave unable to compile.
    let artifacts = match build(options) {
        Err(StubsError::Failed { .. }) if !options.check && blank_generated_glue(options)? => build(options)?,
        built => built?,
    };
    let missing = missing_imports(options, &artifacts)?;
    let (imports, headers) = classify_all(options, &inputs, &missing)?;

    let glue = options.wifi_dir.join(GLUE_DIR);
    let files = [
        (options.lkpi_dir.join(RUST_OUTPUT), render::rust_stubs(&imports)),
        (glue.join(DATA_OUTPUT), render::data_c(&imports, &headers)),
        (glue.join(VARIADIC_OUTPUT), render::variadic_c(&imports)),
    ];
    let changed = publish(&files, options.check)?;

    let mut counts = [0; Class::LABELS.len()];
    for import in &imports {
        let label = import.class.label();
        if let Some(slot) = Class::LABELS.iter().position(|l| *l == label) {
            counts[slot] += 1;
        }
    }
    let unknown = imports.iter().filter(|i| i.class == Class::Unknown).map(|i| i.symbol.clone()).collect();
    Ok(Report { total: imports.len(), counts, unknown, changed })
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};

    use super::*;

    #[test]
    fn kbuild_flags_split_on_tabs_per_source() {
        let flags = parse_unit_flags("net/a.c\t-DX=1\t-DY\nnet/b.c\n");
        check!(flags["net/a.c"] == ["-DX=1", "-DY"]);
        check!(flags["net/b.c"].is_empty());
    }

    #[test]
    fn renames_map_the_shim_name_back_to_libc() {
        let renames = parse_renames("# comment\nsnprintf lx_snprintf\ntimer_delete lx_timer_delete\n");
        check!(renames["lx_snprintf"] == "snprintf");
        check!(renames.len() == 2);
    }

    #[test]
    fn parallel_results_keep_the_order_of_the_items() {
        let items: Vec<usize> = (0..200).collect();
        check!(parallel(&items, |n| n * 2) == items.iter().map(|n| n * 2).collect::<Vec<_>>());
        check!(parallel::<usize, usize>(&[], |n| *n).is_empty());
    }

    #[test]
    fn publishing_writes_only_what_changed_and_check_refuses_to_write() {
        let dir = std::env::temp_dir().join(format!("ferrite-stubs-publish-{}", std::process::id()));
        let (a, b) = (dir.join("sub/a.txt"), dir.join("b.txt"));
        let files = [(a.clone(), "one".to_owned()), (b.clone(), "two".to_owned())];

        assert!(let Err(StubsError::Drift { files: stale }) = publish(&files, true));
        check!(stale.len() == 2);
        check!(!a.exists());

        check!(publish(&files, false).ok() == Some(vec![a, b]));
        check!(publish(&files, false).ok() == Some(Vec::new()));
        check!(publish(&files, true).is_ok());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_report_names_each_class_and_the_unknowns() {
        let report = Report {
            total: 9,
            counts: [4, 1, 2, 1, 0, 1],
            unknown: vec!["__x86_return_thunk".to_owned()],
            changed: Vec::new(),
        };
        let text = report.to_string();
        check!(text.contains("9 imports: function 4, variadic 1, data 2, seam 1, host 0, unknown 1"));
        check!(text.contains("no declaration for __x86_return_thunk"));
        check!(text.ends_with("generated files unchanged"));
    }
}
