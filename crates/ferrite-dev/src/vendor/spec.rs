//! The tracked inputs of `vendor`: the pin and config in `vendor.toml`, `source.list`, `cflags.txt`.

use std::collections::BTreeMap;
use std::path::{Component, Path};

use serde::Deserialize;
use snafu::ensure;

use super::{InvalidSnafu, VendorError};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub pin: Pin,
    pub config: ConfigSpec,
    pub kbuild: KbuildSpec,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pin {
    pub tag: String,
    pub url: String,
    /// Lower-case hex SHA-256 of the tarball.
    pub sha256: String,
}

impl Pin {
    /// The tarball's file name, the last segment of its URL.
    pub fn tarball_name(&self) -> &str {
        self.url.rsplit('/').next().unwrap_or(&self.url)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigSpec {
    pub enable: Vec<String>,
    pub disable: Vec<String>,
    pub expect_set: Vec<String>,
    pub expect_unset: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KbuildSpec {
    pub dirs: Vec<String>,
    pub cut: Vec<String>,
}

impl Spec {
    pub fn parse(text: &str) -> Result<Self, VendorError> {
        let spec: Self = toml::from_str(text)
            .map_err(|e| VendorError::Invalid { what: "vendor.toml".to_owned(), reason: e.to_string() })?;
        let sha = &spec.pin.sha256;
        ensure!(
            sha.len() == 64 && sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
            InvalidSnafu { what: "vendor.toml", reason: "pin.sha256 must be 64 lower-case hex digits" }
        );
        ensure!(
            !spec.pin.tarball_name().is_empty(),
            InvalidSnafu { what: "vendor.toml", reason: "pin.url has no file name" }
        );
        Ok(spec)
    }
}

/// The `.c` files to compile, as paths relative to the Linux root. Blank lines and `#` comments are skipped.
pub fn parse_source_list(text: &str) -> Result<Vec<String>, VendorError> {
    let mut files: Vec<String> = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let path = Path::new(line);
        ensure!(
            path.extension().is_some_and(|ext| ext == "c")
                && path.components().all(|c| matches!(c, Component::Normal(_))),
            InvalidSnafu { what: "source.list", reason: format!("`{line}` is not a relative path to a .c file") }
        );
        ensure!(
            !files.iter().any(|f| f == line),
            InvalidSnafu { what: "source.list", reason: format!("`{line}` is listed twice") }
        );
        files.push(line.to_owned());
    }
    Ok(files)
}

/// The compile arguments of `cflags.txt`: every whitespace-separated token outside `#` comment lines.
pub fn parse_cflags(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .flat_map(str::split_whitespace)
        .map(str::to_owned)
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expect {
    Set { name: String, value: String },
    Unset(String),
}

impl ConfigSpec {
    pub fn expectations(&self) -> Result<Vec<Expect>, VendorError> {
        let set = self.expect_set.iter().map(|entry| match entry.split_once('=') {
            Some((name, value)) if !name.is_empty() => {
                Ok(Expect::Set { name: name.to_owned(), value: value.to_owned() })
            },
            _ => Err(VendorError::Invalid {
                what: "vendor.toml".to_owned(),
                reason: format!("expect_set entry `{entry}` is not NAME=value"),
            }),
        });
        let unset = self.expect_unset.iter().map(|name| Ok(Expect::Unset(name.clone())));
        set.chain(unset).collect()
    }
}

/// The values of a kconfig `.config`; an option that is off is simply absent.
pub fn parse_dotconfig(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(name, _)| name.starts_with("CONFIG_"))
        .map(|(name, value)| (name.to_owned(), value.trim_matches('"').to_owned()))
        .collect()
}

/// Every expectation the `.config` does not meet, as a readable line each.
pub fn unmet(config: &BTreeMap<String, String>, expectations: &[Expect]) -> Vec<String> {
    expectations
        .iter()
        .filter_map(|expect| match expect {
            Expect::Set { name, value } => match config.get(name) {
                Some(got) if got == value => None,
                Some(got) => Some(format!("{name} is {got}, expected {value}")),
                None => Some(format!("{name} is not set, expected {value}")),
            },
            Expect::Unset(name) => config.get(name).map(|got| format!("{name} is {got}, expected it unset")),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};
    use rstest::rstest;

    use super::*;

    const TRACKED_SPEC: &str = include_str!("../../../ferrite-wifi-sys/vendor.toml");
    const TRACKED_FLAGS: &str = include_str!("../../../ferrite-wifi-sys/cflags.txt");
    const TRACKED_LIST: &str = include_str!("../../../ferrite-wifi-sys/source.list");

    #[test]
    fn the_tracked_inputs_parse_and_agree_with_each_other() {
        assert!(let Ok(spec) = Spec::parse(TRACKED_SPEC));
        check!(spec.pin.tarball_name() == "linux-6.18.63.tar.xz");
        assert!(let Ok(expectations) = spec.config.expectations());
        check!(expectations.len() == spec.config.expect_set.len() + spec.config.expect_unset.len());
        assert!(let Ok(files) = parse_source_list(TRACKED_LIST));
        check!(files.iter().all(|f| spec.kbuild.dirs.iter().any(|d| f.starts_with(&format!("{d}/")))));
        check!(spec.kbuild.cut.iter().all(|f| !files.contains(f)));
        check!(parse_cflags(TRACKED_FLAGS).contains(&"-O2".to_owned()));
    }

    #[rstest]
    #[case::absolute("/etc/passwd.c")]
    #[case::climbs("net/../../x.c")]
    #[case::not_c("net/mac80211/main.h")]
    fn bad_source_lines_are_rejected(#[case] line: &str) {
        check!(parse_source_list(line).is_err());
    }

    #[test]
    fn a_source_list_skips_comments_and_refuses_repeats() {
        assert!(let Ok(files) = parse_source_list("# note\n\nnet/a.c\n  net/b.c  \n"));
        check!(files == ["net/a.c", "net/b.c"]);
        check!(parse_source_list("net/a.c\nnet/a.c\n").is_err());
    }

    #[test]
    fn flags_are_split_on_whitespace_and_comments_dropped() {
        check!(parse_cflags("# c\n-include linux/a.h\n  -O2\n") == ["-include", "linux/a.h", "-O2"]);
    }

    #[test]
    fn config_expectations_report_each_miss() {
        let config = parse_dotconfig("# CONFIG_SMP is not set\nCONFIG_PCI=y\nCONFIG_NAME=\"x\"\nCONFIG_PM=y\n");
        let want = [
            Expect::Set { name: "CONFIG_PCI".into(), value: "y".into() },
            Expect::Set { name: "CONFIG_RTW89".into(), value: "y".into() },
            Expect::Set { name: "CONFIG_NAME".into(), value: "y".into() },
            Expect::Unset("CONFIG_SMP".into()),
            Expect::Unset("CONFIG_PM".into()),
        ];
        check!(
            unmet(&config, &want)
                == [
                    "CONFIG_RTW89 is not set, expected y",
                    "CONFIG_NAME is x, expected y",
                    "CONFIG_PM is y, expected it unset"
                ]
        );
    }
}
