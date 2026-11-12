//! `vendor`: fetch the pinned Linux release and cut the snapshot `ferrite-wifi-sys` compiles.
//!
//! The tarball is cached and checked against the pinned SHA-256. The full tree is extracted into a
//! work directory only long enough to configure Kbuild, evaluate which C files it builds and find the
//! headers they read; it is deleted afterwards. The snapshot lands in the crate's untracked `linux/`.

pub(crate) mod fetch;
#[path = "../../ferrite-wifi-sys/vendor_inputs.rs"]
mod inputs;
mod kbuild;
mod kernel;
mod snapshot;
pub(crate) mod spec;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use snafu::{ResultExt, Snafu, ensure};

use kernel::Tree;
use snapshot::{FLAGS_FILE, SNAPSHOT_DIR, VERSION_FILE};
use spec::Spec;

/// The LLVM tools Kbuild, the header scan and the build script use, unless `FERRITE_LLVM` says otherwise.
pub const DEFAULT_LLVM: &str = "/usr/lib/llvm-21/bin";

#[derive(Debug, Snafu)]
pub enum VendorError {
    #[snafu(display("neither FERRITE_CACHE, XDG_CACHE_HOME nor HOME is set, so there is no cache directory"))]
    NoHome,

    #[snafu(display("cannot {what} {}", path.display()))]
    Io { what: &'static str, path: PathBuf, source: io::Error },

    #[snafu(display("cannot run {program}"))]
    Spawn { program: String, source: io::Error },

    #[snafu(display("could not {what} ({status}):\n{detail}"))]
    Failed { what: String, status: String, detail: String },

    #[snafu(display("{what} is invalid: {reason}"))]
    Invalid { what: String, reason: String },

    #[snafu(display("{file} hashes to {got}, but the pin says {want}"))]
    Hash { file: String, want: String, got: String },

    #[snafu(display("kconfig dropped options the build needs:\n  {}", unmet.join("\n  ")))]
    Config { unmet: Vec<String> },

    #[snafu(display("{dir}/Makefile builds {object}, which is not a .c file in the tree"))]
    NotC { dir: String, object: String },

    #[snafu(display(
        "source.list and Kbuild disagree (Kbuild's list is in the cache as kbuild.list)\n  missing from source.list: {}\n  listed but not built by Kbuild: {}",
        join_or_none(missing),
        join_or_none(extra)
    ))]
    Drift { missing: Vec<String>, extra: Vec<String> },

    #[snafu(display("{file} reads {header}, which is not below the Linux root"))]
    Header { header: String, file: String },
}

fn join_or_none(items: &[String]) -> String {
    if items.is_empty() { "none".to_owned() } else { items.join(", ") }
}

#[derive(Debug, Clone)]
pub struct VendorOptions {
    /// The `ferrite-wifi-sys` directory, which holds the tracked inputs and receives the snapshot.
    pub crate_dir: PathBuf,
    pub cache_dir: PathBuf,
    /// Directory of the LLVM tools.
    pub llvm: PathBuf,
    /// Regenerate even when the snapshot's inputs have not changed.
    pub force: bool,
}

impl VendorOptions {
    /// Options from the environment: `FERRITE_CACHE` and `FERRITE_LLVM` override their defaults.
    pub fn from_env(crate_dir: PathBuf, force: bool) -> Result<Self, VendorError> {
        let cache_dir = fetch::cache_dir(|name| std::env::var_os(name))?;
        let llvm = std::env::var_os("FERRITE_LLVM")
            .filter(|v| !v.is_empty())
            .map_or_else(|| DEFAULT_LLVM.into(), PathBuf::from);
        Ok(Self { crate_dir, cache_dir, llvm, force })
    }
}

/// Run `command` to completion; its stdout on success, else the tail of what it printed.
fn exec(command: &mut Command, what: &str) -> Result<String, VendorError> {
    let program = command.get_program().to_string_lossy().into_owned();
    let output = command.output().context(SpawnSnafu { program })?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let text = if stderr.trim().is_empty() { String::from_utf8_lossy(&output.stdout) } else { stderr };
    let lines: Vec<&str> = text.lines().collect();
    let detail = lines[lines.len().saturating_sub(20)..].join("\n");
    FailedSnafu { what, status: output.status.to_string(), detail }.fail()
}

/// Removes the work directory when dropped, on success and on failure alike.
struct Scratch(PathBuf);

impl Scratch {
    fn new(path: PathBuf) -> Result<Self, VendorError> {
        remove_dir(&path)?;
        fs::create_dir_all(&path).context(IoSnafu { what: "create", path: &path })?;
        Ok(Self(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Err(e) = remove_dir(&self.0) {
            eprintln!("ferrite-dev: {e}");
        }
    }
}

fn remove_dir(path: &Path) -> Result<(), VendorError> {
    match fs::remove_dir_all(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e).context(IoSnafu { what: "remove", path }),
        _ => Ok(()),
    }
}

fn read(path: &Path) -> Result<String, VendorError> {
    fs::read_to_string(path).context(IoSnafu { what: "read", path })
}

pub fn run(options: &VendorOptions) -> Result<(), VendorError> {
    let crate_dir = &options.crate_dir;
    let snapshot = crate_dir.join(SNAPSHOT_DIR);
    let digest = inputs::digest(crate_dir).context(IoSnafu { what: "read the inputs in", path: crate_dir })?;
    let current = fs::read_to_string(snapshot.join(VERSION_FILE)).ok();
    if !options.force && current.as_deref().is_some_and(|v| v.lines().any(|line| line == inputs::version_line(&digest)))
    {
        let tag = current.as_deref().and_then(|v| v.lines().next()).unwrap_or_default();
        println!("vendor: up to date ({tag})");
        return Ok(());
    }

    let spec = Spec::parse(&read(&crate_dir.join("vendor.toml"))?)?;
    let listed = spec::parse_source_list(&read(&crate_dir.join("source.list"))?)?;
    let cflags = spec::parse_cflags(&read(&crate_dir.join("cflags.txt"))?);
    let patches = inputs::patch_files(crate_dir).context(IoSnafu { what: "list the patches in", path: crate_dir })?;
    let version = format!(
        "tag {}\ntarball {}\nsha256 {}\n{}\n",
        spec.pin.tag,
        spec.pin.tarball_name(),
        spec.pin.sha256,
        inputs::version_line(&digest)
    );

    let tarball = fetch::ensure_tarball(&options.cache_dir, &spec.pin)?;
    let work = Scratch::new(options.cache_dir.join("work"))?;
    let tree = Tree { src: work.0.join("tree"), obj: work.0.join("kbuild"), llvm: options.llvm.clone() };

    println!("vendor: extracting {}", spec.pin.tarball_name());
    kernel::extract(&tarball, &tree)?;
    kernel::apply_patches(&tree, &patches)?;
    println!("vendor: configuring");
    kernel::configure(&tree, &spec.config)?;

    println!("vendor: evaluating Kbuild");
    let kbuild = kbuild::evaluate(&tree, &work.0, &spec.kbuild.dirs)?;
    let kbuild_list = kbuild
        .iter()
        .filter(|f| !spec.kbuild.cut.contains(&f.source))
        .fold(String::new(), |list, f| list + &f.source + "\n");
    let list_path = options.cache_dir.join("kbuild.list");
    fs::write(&list_path, kbuild_list).context(IoSnafu { what: "write", path: &list_path })?;
    let drift = kbuild::drift(&kbuild, &spec.kbuild.cut, &listed, &spec.kbuild.dirs);
    ensure!(drift.is_empty(), DriftSnafu { missing: drift.missing, extra: drift.extra });

    println!("vendor: scanning the headers of {} files", listed.len());
    let view = work.0.join("view");
    snapshot::link_view(&view, &tree)?;
    let units = snapshot::units(&listed, &kbuild);
    let files = snapshot::closure(&view, &options.llvm, &cflags, &units)?;

    let staging = crate_dir.join(format!("{SNAPSHOT_DIR}.new"));
    remove_dir(&staging)?;
    snapshot::write(&staging, &view, &files, &units, &version)?;
    remove_dir(&snapshot)?;
    fs::rename(&staging, &snapshot).context(IoSnafu { what: "rename", path: &staging })?;
    println!(
        "vendor: wrote {} ({} files, {} MB), per-file flags in {FLAGS_FILE}",
        snapshot.display(),
        files.len(),
        snapshot::size(&snapshot) >> 20
    );
    Ok(())
}
