//! Machine-specific settings, read from a file outside the repository.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use snafu::{ResultExt, Snafu, ensure};

#[derive(Debug, Snafu)]
pub enum ConfigError {
    #[snafu(display("cannot read config {}", path.display()))]
    Read { path: PathBuf, source: io::Error },

    #[snafu(display("invalid config {}", path.display()))]
    Parse { path: PathBuf, source: toml::de::Error },

    #[snafu(display("config value `{field}` is not usable in a remote command line: {value:?}"))]
    UnsafeValue { field: &'static str, value: String },

    #[snafu(display("cannot render the config"))]
    Render { source: toml::ser::Error },

    #[snafu(display("neither XDG_CONFIG_HOME nor HOME is set, so there is no default config location"))]
    NoHome,
}

/// Every value that differs between machines. A missing key keeps its default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// The target laptop as `ssh` and `scp` know it.
    pub ssh_host: String,
    /// The tty the DbC link appears as on this machine.
    pub device: PathBuf,
    /// The sysfs directory of the USB port the DbC cable is plugged into. Unset by default, and
    /// port re-enabling is skipped while it is unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port_path: Option<PathBuf>,
    /// The root helper on the target, run through `sudo -n`.
    pub helper: String,
    /// The helper's staging directory on the target, relative to its home.
    pub stage_dir: String,
    /// Where `run` creates one directory per cycle.
    pub run_dir: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ssh_host: "target".into(),
            device: "/dev/ferrite-dbc".into(),
            port_path: None,
            helper: "/usr/local/sbin/ferrite-esp".into(),
            stage_dir: "ferrite-stage".into(),
            run_dir: "target/ferrite-dev/runs".into(),
        }
    }
}

impl Config {
    /// Parse `text`, which came from `path`; keys left out keep their defaults.
    pub fn parse(text: &str, path: &Path) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(text).context(ParseSnafu { path })?;
        config.validate()?;
        Ok(config)
    }

    /// Read the file at `path`. A missing file gives the defaults unless `required`.
    pub fn load(path: &Path, required: bool) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text, path),
            Err(e) if e.kind() == io::ErrorKind::NotFound && !required => Ok(Self::default()),
            Err(source) => Err(ConfigError::Read { path: path.to_owned(), source }),
        }
    }

    pub fn render(&self) -> Result<String, ConfigError> {
        toml::to_string(self).context(RenderSnafu)
    }

    /// The values that end up inside `ssh` and `scp` arguments must not look like options or
    /// carry shell syntax, since the remote side re-parses them.
    fn validate(&self) -> Result<(), ConfigError> {
        for (field, value) in [
            ("ssh_host", self.ssh_host.as_str()),
            ("helper", self.helper.as_str()),
            ("stage_dir", self.stage_dir.as_str()),
        ] {
            ensure!(remote_safe(value), UnsafeValueSnafu { field, value });
        }
        let stage = self.stage_dir.as_str();
        ensure!(!stage.split('/').any(|part| part == ".."), UnsafeValueSnafu { field: "stage_dir", value: stage });
        Ok(())
    }
}

fn remote_safe(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && value.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '@' | ':' | '~' | '-'))
}

/// `$XDG_CONFIG_HOME/ferrite-dev/config.toml`, else `$HOME/.config/ferrite-dev/config.toml`.
pub fn default_path(var: impl Fn(&str) -> Option<OsString>) -> Result<PathBuf, ConfigError> {
    let base = match var("XDG_CONFIG_HOME").filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(var("HOME").filter(|dir| !dir.is_empty()).ok_or(ConfigError::NoHome)?).join(".config"),
    };
    Ok(base.join("ferrite-dev").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};
    use rstest::rstest;

    use super::*;

    #[test]
    fn defaults_are_todays_values() {
        let config = Config::default();
        check!(config.ssh_host == "target");
        check!(config.device == Path::new("/dev/ferrite-dbc"));
        check!(config.helper == "/usr/local/sbin/ferrite-esp");
        check!(config.stage_dir == "ferrite-stage");
        check!(config.port_path.is_none());
    }

    #[test]
    fn an_empty_file_is_all_defaults() {
        assert!(let Ok(config) = Config::parse("", Path::new("c.toml")));
        check!(config == Config::default());
    }

    #[test]
    fn a_partial_file_overrides_only_what_it_names() {
        let text = "ssh_host = \"spare\"\nport_path = \"/sys/other\"\n";
        assert!(let Ok(config) = Config::parse(text, Path::new("c.toml")));
        check!(config.ssh_host == "spare");
        check!(config.port_path.as_deref() == Some(Path::new("/sys/other")));
        check!(config.device == Config::default().device);
        check!(config.helper == Config::default().helper);
    }

    #[rstest]
    #[case::typo("ssh_hots = \"x\"")]
    #[case::wrong_type("device = 4")]
    #[case::option_lookalike("ssh_host = \"-oProxyCommand=x\"")]
    #[case::shell_syntax("stage_dir = \"a; rm -rf ~\"")]
    #[case::empty_helper("helper = \"\"")]
    #[case::stage_climbs_out("stage_dir = \"../elsewhere\"")]
    #[case::stage_climbs_out_midway("stage_dir = \"a/../../b\"")]
    #[case::stage_is_the_parent("stage_dir = \"..\"")]
    #[case::stage_glob("stage_dir = \"stage*\"")]
    fn bad_files_are_rejected(#[case] text: &str) {
        check!(Config::parse(text, Path::new("c.toml")).is_err());
    }

    #[rstest]
    #[case::dots_in_a_name("a..b/c")]
    #[case::absolute("/srv/ferrite-stage")]
    #[case::current_dir(".")]
    fn ordinary_stage_dirs_are_accepted(#[case] stage: &str) {
        let config = Config::parse(&format!("stage_dir = \"{stage}\""), Path::new("c.toml"));
        check!(config.is_ok());
    }

    #[test]
    fn rendered_config_parses_back_to_itself() {
        let config = Config { ssh_host: "other".into(), ..Config::default() };
        assert!(let Ok(text) = config.render());
        check!(Config::parse(&text, Path::new("c.toml")).ok() == Some(config));
    }

    #[rstest]
    #[case::xdg(Some("/x"), Some("/home/u"), Some("/x/ferrite-dev/config.toml"))]
    #[case::home(None, Some("/home/u"), Some("/home/u/.config/ferrite-dev/config.toml"))]
    #[case::empty_xdg(Some(""), Some("/home/u"), Some("/home/u/.config/ferrite-dev/config.toml"))]
    #[case::neither(None, None, None)]
    fn default_location(#[case] xdg: Option<&str>, #[case] home: Option<&str>, #[case] want: Option<&str>) {
        let var = |name: &str| match name {
            "XDG_CONFIG_HOME" => xdg.map(OsString::from),
            "HOME" => home.map(OsString::from),
            _ => None,
        };
        check!(default_path(var).ok() == want.map(PathBuf::from));
    }

    #[test]
    fn a_missing_file_is_defaults_unless_required() {
        let path = Path::new("/nonexistent-ferrite-dev-dir/config.toml");
        check!(Config::load(path, false).ok() == Some(Config::default()));
        check!(Config::load(path, true).is_err());
    }
}
