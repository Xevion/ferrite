//! The generator against the real Linux archive: the tracked generated files must be exactly what a fresh
//! run produces, which is also what makes a rerun with no input change a no-op. Needs the snapshot from
//! `just vendor`, the LLVM tools and the pinned tarball in the cache. It takes most of a minute of every
//! core, so it is ignored by default; `just stubs --check` runs the same check.

use std::path::Path;

use assert2::{assert, check};
use ferrite_dev::stubs::{StubsOptions, run};

#[test]
#[ignore = "slow: rebuilds the archive and runs clang once per import; `just stubs --check` is the same check"]
fn a_rerun_reproduces_the_tracked_generated_files() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    assert!(let Ok(options) = StubsOptions::from_env(crates.join("ferrite-wifi-sys"), crates.join("ferrite-lkpi"), true));
    assert!(let Ok(report) = run(&options));
    check!(report.total > 300, "the imports were not found: {report}");
    check!(report.changed.is_empty());
}
