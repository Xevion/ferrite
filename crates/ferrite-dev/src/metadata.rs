//! Finding which workspace package owns a binary, from `cargo metadata` output.

use std::path::PathBuf;

use serde::Deserialize;
use snafu::{ResultExt, Snafu};

#[derive(Debug, Snafu)]
pub enum MetadataError {
    #[snafu(display("cannot parse `cargo metadata` output"))]
    Parse { source: serde_json::Error },

    #[snafu(display("no binary named `{app}` in the workspace (binaries: {})", known.join(", ")))]
    UnknownBin { app: String, known: Vec<String> },

    #[snafu(display("binary `{app}` is defined by several packages: {}", packages.join(", ")))]
    AmbiguousBin { app: String, packages: Vec<String> },
}

#[derive(Debug, Deserialize)]
pub struct Metadata {
    packages: Vec<Package>,
    pub target_directory: PathBuf,
}

#[derive(Debug, Deserialize)]
struct Package {
    name: String,
    targets: Vec<Target>,
}

#[derive(Debug, Deserialize)]
struct Target {
    name: String,
    kind: Vec<String>,
}

impl Metadata {
    pub fn parse(json: &str) -> Result<Self, MetadataError> {
        serde_json::from_str(json).context(ParseSnafu)
    }

    fn bins(&self) -> impl Iterator<Item = (&str, &str)> {
        self.packages.iter().flat_map(|package| {
            package
                .targets
                .iter()
                .filter(|target| target.kind.iter().any(|kind| kind == "bin"))
                .map(|target| (package.name.as_str(), target.name.as_str()))
        })
    }

    /// The package that defines the binary `app`.
    pub fn package_of_bin(&self, app: &str) -> Result<&str, MetadataError> {
        let mut owners = self.bins().filter(|&(_, bin)| bin == app).map(|(package, _)| package);
        match (owners.next(), owners.next()) {
            (Some(package), None) => Ok(package),
            (None, _) => {
                let mut known: Vec<String> = self.bins().map(|(_, bin)| bin.to_owned()).collect();
                known.sort();
                Err(MetadataError::UnknownBin { app: app.to_owned(), known })
            },
            (Some(first), Some(second)) => Err(MetadataError::AmbiguousBin {
                app: app.to_owned(),
                packages: [first, second].into_iter().chain(owners).map(str::to_owned).collect(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};
    use rstest::rstest;

    use super::*;

    const FIXTURE: &str = r#"{
        "packages": [
            {"name": "ferrite-recon", "id": "path+file:///w/crates/ferrite-recon#0.1.0", "version": "0.1.0",
             "targets": [
                {"name": "ferrite_recon", "kind": ["lib"], "crate_types": ["lib"]},
                {"name": "pciscan", "kind": ["bin"], "crate_types": ["bin"]},
                {"name": "dbcbench", "kind": ["bin"], "crate_types": ["bin"]},
                {"name": "dbcping", "kind": ["bin"], "crate_types": ["bin"]}]},
            {"name": "ferrite-uefi", "id": "path+file:///w/crates/ferrite-uefi#0.1.0", "version": "0.1.0",
             "targets": [
                {"name": "ferrite-uefi", "kind": ["bin"], "crate_types": ["bin"]},
                {"name": "ferrite_uefi", "kind": ["lib"], "crate_types": ["lib"]}]},
            {"name": "ferrite-wire", "id": "path+file:///w/crates/ferrite-wire#0.1.0", "version": "0.1.0",
             "targets": [{"name": "ferrite_wire", "kind": ["lib"], "crate_types": ["lib"]},
                         {"name": "demo", "kind": ["example"], "crate_types": ["bin"]}]}
        ],
        "workspace_members": [],
        "target_directory": "/w/target",
        "version": 1
    }"#;

    #[rstest]
    #[case::recon_bin("dbcbench", "ferrite-recon")]
    #[case::another_recon_bin("pciscan", "ferrite-recon")]
    #[case::bin_named_like_its_package("ferrite-uefi", "ferrite-uefi")]
    fn the_owning_package_is_found(#[case] app: &str, #[case] want: &str) {
        assert!(let Ok(metadata) = Metadata::parse(FIXTURE));
        check!(metadata.package_of_bin(app).ok() == Some(want));
        check!(metadata.target_directory == PathBuf::from("/w/target"));
    }

    #[rstest]
    #[case::a_library("ferrite_recon")]
    #[case::an_example("demo")]
    #[case::a_typo("dbcbenh")]
    fn only_binaries_count(#[case] app: &str) {
        assert!(let Ok(metadata) = Metadata::parse(FIXTURE));
        assert!(let Err(MetadataError::UnknownBin { known, .. }) = metadata.package_of_bin(app));
        check!(known == ["dbcbench", "dbcping", "ferrite-uefi", "pciscan"]);
    }

    #[test]
    fn a_binary_in_two_packages_is_ambiguous() {
        let json = r#"{"target_directory": "/t", "packages": [
            {"name": "a", "targets": [{"name": "tool", "kind": ["bin"]}]},
            {"name": "b", "targets": [{"name": "tool", "kind": ["bin"]}]}]}"#;
        assert!(let Ok(metadata) = Metadata::parse(json));
        assert!(let Err(MetadataError::AmbiguousBin { packages, .. }) = metadata.package_of_bin("tool"));
        check!(packages == ["a", "b"]);
    }

    #[test]
    fn garbage_is_a_parse_error() {
        check!(Metadata::parse("not json").is_err());
    }
}
