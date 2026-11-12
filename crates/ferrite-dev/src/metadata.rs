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

    #[snafu(display(
        "package `{package}` sets metadata.ferrite.target to `{target}`; use {UEFI_TRIPLE} or {NONE_TRIPLE}"
    ))]
    UnknownTarget { package: String, target: String },
}

const UEFI_TRIPLE: &str = "x86_64-unknown-uefi";
const NONE_TRIPLE: &str = "x86_64-unknown-none";

/// What an app is built for, from `[package.metadata.ferrite] target` in its manifest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppTarget {
    /// The default: `x86_64-unknown-uefi`, whose linker writes the `.efi` itself.
    Uefi,
    /// `x86_64-unknown-none`, for apps that link ELF objects; the build converts the ELF into the `.efi`.
    None,
}

impl AppTarget {
    pub const fn triple(self) -> &'static str {
        match self {
            Self::Uefi => UEFI_TRIPLE,
            Self::None => NONE_TRIPLE,
        }
    }
}

/// The package that defines an app binary, and what it is built for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct App<'a> {
    pub package: &'a str,
    pub target: AppTarget,
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
    #[serde(default)]
    metadata: Option<PackageMetadata>,
}

#[derive(Debug, Deserialize)]
struct PackageMetadata {
    #[serde(default)]
    ferrite: Option<FerriteMetadata>,
}

#[derive(Debug, Deserialize)]
struct FerriteMetadata {
    target: Option<String>,
}

impl Package {
    fn app_target(&self) -> Result<AppTarget, MetadataError> {
        let target = self.metadata.as_ref().and_then(|m| m.ferrite.as_ref()).and_then(|f| f.target.as_deref());
        match target {
            None | Some(UEFI_TRIPLE) => Ok(AppTarget::Uefi),
            Some(NONE_TRIPLE) => Ok(AppTarget::None),
            Some(other) => Err(MetadataError::UnknownTarget { package: self.name.clone(), target: other.to_owned() }),
        }
    }
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

    fn bins(&self) -> impl Iterator<Item = (&Package, &str)> {
        self.packages.iter().flat_map(|package| {
            package
                .targets
                .iter()
                .filter(|target| target.kind.iter().any(|kind| kind == "bin"))
                .map(move |target| (package, target.name.as_str()))
        })
    }

    /// The package that defines the binary `app`, and the target it is built for.
    pub fn app(&self, app: &str) -> Result<App<'_>, MetadataError> {
        let mut owners = self.bins().filter(|&(_, bin)| bin == app).map(|(package, _)| package);
        match (owners.next(), owners.next()) {
            (Some(package), None) => Ok(App { package: &package.name, target: package.app_target()? }),
            (None, _) => {
                let mut known: Vec<String> = self.bins().map(|(_, bin)| bin.to_owned()).collect();
                known.sort();
                Err(MetadataError::UnknownBin { app: app.to_owned(), known })
            },
            (Some(first), Some(second)) => Err(MetadataError::AmbiguousBin {
                app: app.to_owned(),
                packages: [first, second].into_iter().chain(owners).map(|p| p.name.clone()).collect(),
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
            {"name": "ferrite-wifi-efi", "id": "path+file:///w/crates/ferrite-wifi-efi#0.1.0", "version": "0.1.0",
             "metadata": {"ferrite": {"target": "x86_64-unknown-none"}},
             "targets": [{"name": "wifitrace", "kind": ["bin"], "crate_types": ["bin"]}]},
            {"name": "ferrite-wire", "id": "path+file:///w/crates/ferrite-wire#0.1.0", "version": "0.1.0",
             "metadata": null,
             "targets": [{"name": "ferrite_wire", "kind": ["lib"], "crate_types": ["lib"]},
                         {"name": "demo", "kind": ["example"], "crate_types": ["bin"]}]}
        ],
        "workspace_members": [],
        "target_directory": "/w/target",
        "version": 1
    }"#;

    #[rstest]
    #[case::recon_bin("dbcbench", "ferrite-recon", AppTarget::Uefi)]
    #[case::another_recon_bin("pciscan", "ferrite-recon", AppTarget::Uefi)]
    #[case::bin_named_like_its_package("ferrite-uefi", "ferrite-uefi", AppTarget::Uefi)]
    #[case::built_as_elf("wifitrace", "ferrite-wifi-efi", AppTarget::None)]
    fn the_owning_package_and_its_target_are_found(
        #[case] app: &str,
        #[case] package: &str,
        #[case] target: AppTarget,
    ) {
        assert!(let Ok(metadata) = Metadata::parse(FIXTURE));
        check!(metadata.app(app).ok() == Some(App { package, target }));
        check!(metadata.target_directory == PathBuf::from("/w/target"));
    }

    #[rstest]
    #[case::a_library("ferrite_recon")]
    #[case::an_example("demo")]
    #[case::a_typo("dbcbenh")]
    fn only_binaries_count(#[case] app: &str) {
        assert!(let Ok(metadata) = Metadata::parse(FIXTURE));
        assert!(let Err(MetadataError::UnknownBin { known, .. }) = metadata.app(app));
        check!(known == ["dbcbench", "dbcping", "ferrite-uefi", "pciscan", "wifitrace"]);
    }

    #[test]
    fn a_binary_in_two_packages_is_ambiguous() {
        let json = r#"{"target_directory": "/t", "packages": [
            {"name": "a", "targets": [{"name": "tool", "kind": ["bin"]}]},
            {"name": "b", "targets": [{"name": "tool", "kind": ["bin"]}]}]}"#;
        assert!(let Ok(metadata) = Metadata::parse(json));
        assert!(let Err(MetadataError::AmbiguousBin { packages, .. }) = metadata.app("tool"));
        check!(packages == ["a", "b"]);
    }

    #[test]
    fn an_unknown_target_is_refused() {
        let json = r#"{"target_directory": "/t", "packages": [
            {"name": "a", "metadata": {"ferrite": {"target": "aarch64-unknown-uefi"}},
             "targets": [{"name": "tool", "kind": ["bin"]}]}]}"#;
        assert!(let Ok(metadata) = Metadata::parse(json));
        assert!(let Err(MetadataError::UnknownTarget { target, .. }) = metadata.app("tool"));
        check!(target == "aarch64-unknown-uefi");
    }

    #[test]
    fn garbage_is_a_parse_error() {
        check!(Metadata::parse("not json").is_err());
    }
}
