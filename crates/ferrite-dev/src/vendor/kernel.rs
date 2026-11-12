//! The full Linux tree in the work directory: extract, patch, configure and generate headers.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use snafu::ResultExt;

use super::spec::{self, ConfigSpec};
use super::{ConfigSnafu, IoSnafu, VendorError, exec};

/// An extracted source tree and the separate output tree holding its `.config` and generated headers.
#[derive(Debug, Clone)]
pub struct Tree {
    pub src: PathBuf,
    pub obj: PathBuf,
    /// Directory of the LLVM tools, which Kbuild is told to use for the host and target compiler.
    pub llvm: PathBuf,
}

impl Tree {
    /// A `make` that works on this tree, run from the output directory.
    fn make(&self, goals: &[&str]) -> Command {
        let mut make = Command::new("make");
        make.arg("-C").arg(&self.src).arg("-s").arg("ARCH=x86_64");
        make.arg(with_prefix("O=", &self.obj, ""));
        make.arg(with_prefix("LLVM=", &self.llvm, "/"));
        make.arg(format!("-j{}", std::thread::available_parallelism().map_or(1, usize::from)));
        make.args(goals);
        make
    }

    pub fn dotconfig(&self) -> PathBuf {
        self.obj.join(".config")
    }
}

fn with_prefix(prefix: &str, path: &Path, suffix: &str) -> OsString {
    let mut arg = OsString::from(prefix);
    arg.push(path);
    arg.push(suffix);
    arg
}

/// Unpack `tarball` into `tree.src`, dropping its top-level directory.
pub fn extract(tarball: &Path, tree: &Tree) -> Result<(), VendorError> {
    fs::create_dir_all(&tree.src).context(IoSnafu { what: "create", path: &tree.src })?;
    let mut tar = Command::new("tar");
    tar.arg("-xJf").arg(tarball).arg("-C").arg(&tree.src).arg("--strip-components=1");
    exec(&mut tar, "extract the Linux tarball").map(drop)
}

/// Apply `patches` (already in order) to the tree with `patch -p1`.
pub fn apply_patches(tree: &Tree, patches: &[PathBuf]) -> Result<(), VendorError> {
    for patch in patches {
        let mut command = Command::new("patch");
        command.arg("-p1").arg("-d").arg(&tree.src).arg("-i").arg(patch);
        exec(&mut command, &format!("apply {}", patch.display()))?;
    }
    Ok(())
}

/// Write the config, check that it kept the options we need, and generate the headers Kbuild makes in
/// `prepare0`, then the CRC table header. That target stops before building objtool, which would need libelf.
pub fn configure(tree: &Tree, config: &ConfigSpec) -> Result<(), VendorError> {
    fs::create_dir_all(&tree.obj).context(IoSnafu { what: "create", path: &tree.obj })?;
    exec(&mut tree.make(&["tinyconfig"]), "make tinyconfig")?;

    let mut edit = Command::new(tree.src.join("scripts/config"));
    edit.arg("--file").arg(tree.dotconfig());
    for name in &config.enable {
        edit.args(["--enable", name]);
    }
    for name in &config.disable {
        edit.args(["--disable", name]);
    }
    exec(&mut edit, "scripts/config")?;
    exec(&mut tree.make(&["olddefconfig"]), "make olddefconfig")?;

    let path = tree.dotconfig();
    let text = fs::read_to_string(&path).context(IoSnafu { what: "read", path: &path })?;
    let unmet = spec::unmet(&spec::parse_dotconfig(&text), &config.expectations()?);
    if !unmet.is_empty() {
        return ConfigSnafu { unmet }.fail();
    }
    exec(&mut tree.make(&["prepare0"]), "make prepare0")?;
    generate_crc32_table(tree)
}

/// `lib/crc/crc32table.h`, which Kbuild makes only while building `crc32-main.o` (and that needs objtool).
/// The generator is built and run here with the same compiler.
fn generate_crc32_table(tree: &Tree) -> Result<(), VendorError> {
    let out_dir = tree.obj.join("lib/crc");
    fs::create_dir_all(&out_dir).context(IoSnafu { what: "create", path: &out_dir })?;
    let generator = out_dir.join("gen_crc32table");
    let mut compile = Command::new(tree.llvm.join("clang"));
    compile.arg("-I").arg(&out_dir).arg("-o").arg(&generator).arg(tree.src.join("lib/crc/gen_crc32table.c"));
    exec(&mut compile, "compile gen_crc32table")?;
    let table = exec(&mut Command::new(&generator), "run gen_crc32table")?;
    let header = out_dir.join("crc32table.h");
    fs::write(&header, table).context(IoSnafu { what: "write", path: &header })
}
