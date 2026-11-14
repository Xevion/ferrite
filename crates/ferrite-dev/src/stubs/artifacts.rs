//! Finding the build products the generator reads: the Linux C archive and the shim's rlib. Cargo reports
//! both in its JSON messages, so no path inside `target/` is ever guessed.

use std::path::PathBuf;

use serde::Deserialize;
use snafu::{OptionExt, ResultExt};

use super::{JsonSnafu, MissingSnafu, StubsError};

/// The environment variable `ferrite-wifi-sys`'s build script sets to the archive's path.
const ARCHIVE_VAR: &str = "FERRITE_WIFI_ARCHIVE";

#[derive(Debug, PartialEq, Eq)]
pub struct Artifacts {
    /// `libferrite_wifi_c.a`: the Linux objects, the initcall tables and the glue.
    pub archive: PathBuf,
    /// The shim crate's library, built without its generated stubs.
    pub rlib: PathBuf,
}

#[derive(Debug, Deserialize)]
struct Message {
    reason: String,
    #[serde(default)]
    package_id: String,
    target: Option<Target>,
    filenames: Option<Vec<PathBuf>>,
    env: Option<Vec<(String, String)>>,
}

#[derive(Debug, Deserialize)]
struct Target {
    name: String,
    crate_types: Vec<String>,
}

/// The Linux archive's path in the output of a cargo build that ran the build script of `wifi_sys`.
pub fn archive_path(stdout: &str, wifi_sys: &str) -> Option<PathBuf> {
    stdout
        .lines()
        .filter(|line| line.starts_with('{'))
        .filter_map(|line| serde_json::from_str::<Message>(line).ok())
        .filter(|message| message.reason == "build-script-executed" && message.package_id.contains(wifi_sys))
        .find_map(|message| message.env?.into_iter().find(|(name, _)| name == ARCHIVE_VAR))
        .map(|(_, path)| PathBuf::from(path))
}

/// The artifacts in the output of `cargo build --message-format=json` for `shim` and `wifi_sys`.
pub fn parse(stdout: &str, shim: &str, wifi_sys: &str) -> Result<Artifacts, StubsError> {
    let (mut archive, mut rlib) = (None, None);
    for line in stdout.lines().filter(|line| line.starts_with('{')) {
        let message: Message = serde_json::from_str(line).context(JsonSnafu { what: "a cargo message" })?;
        match message.reason.as_str() {
            "build-script-executed" if message.package_id.contains(wifi_sys) => {
                let value = message.env.into_iter().flatten().find(|(name, _)| name == ARCHIVE_VAR);
                archive = value.map(|(_, path)| PathBuf::from(path));
            },
            "compiler-artifact" => {
                let is_shim = message
                    .target
                    .is_some_and(|t| t.name == shim.replace('-', "_") && t.crate_types.iter().any(|c| c == "lib"));
                if is_shim {
                    rlib = message
                        .filenames
                        .into_iter()
                        .flatten()
                        .find(|path| path.extension().is_some_and(|e| e == "rlib"));
                }
            },
            _ => {},
        }
    }
    Ok(Artifacts {
        archive: archive.context(MissingSnafu { what: format!("{ARCHIVE_VAR} in the build of {wifi_sys}") })?,
        rlib: rlib.context(MissingSnafu { what: format!("the rlib of {shim}") })?,
    })
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};

    use super::*;

    const OUTPUT: &str = r#"
{"reason":"compiler-artifact","package_id":"path+file:///w/crates/other#0.1.0","target":{"name":"other","crate_types":["lib"]},"filenames":["/t/libother.rlib"]}
{"reason":"build-script-executed","package_id":"path+file:///w/crates/ferrite-wifi-sys#0.1.0","env":[["FERRITE_WIFI_INITCALLS","/t/initcalls.c"],["FERRITE_WIFI_ARCHIVE","/t/libferrite_wifi_c.a"]],"out_dir":"/t"}
{"reason":"compiler-artifact","package_id":"path+file:///w/crates/ferrite-lkpi#0.1.0","target":{"name":"ferrite_lkpi","crate_types":["lib"]},"filenames":["/t/libferrite_lkpi.rlib","/t/libferrite_lkpi.rmeta"]}
{"reason":"build-finished","success":true}
"#;

    #[test]
    fn the_archive_and_the_rlib_are_read_from_cargo_messages() {
        assert!(let Ok(found) = parse(OUTPUT, "ferrite-lkpi", "ferrite-wifi-sys"));
        check!(found.archive == PathBuf::from("/t/libferrite_wifi_c.a"));
        check!(found.rlib == PathBuf::from("/t/libferrite_lkpi.rlib"));
    }

    #[test]
    fn the_archive_alone_is_read_from_cargo_messages() {
        check!(archive_path(OUTPUT, "ferrite-wifi-sys") == Some(PathBuf::from("/t/libferrite_wifi_c.a")));
        check!(archive_path(OUTPUT, "ferrite-other").is_none());
    }

    #[test]
    fn a_missing_artifact_is_an_error_naming_it() {
        let without_rlib: String =
            OUTPUT.lines().filter(|l| !l.contains("ferrite-lkpi")).collect::<Vec<_>>().join("\n");
        assert!(let Err(error) = parse(&without_rlib, "ferrite-lkpi", "ferrite-wifi-sys"));
        check!(error.to_string().contains("rlib of ferrite-lkpi"));
    }
}
