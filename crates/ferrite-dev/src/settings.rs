//! `settings`: write the stick's `ferrite.toml`, from the repo's `.env` or as the commented template.
//!
//! The `.env` lines are the developer's own credentials, so nothing here prints a value: an error names
//! the `.env` key and its line. The text is converted to the plain data of `ferrite-config`, rendered,
//! and read back through `ferrite_config::load`, so a file that would be refused on the stick is refused
//! here, by the same checks. The `[time]` zone is the host's own, read from where the system keeps it
//! ([`zone_name`]), since a file for a machine on the same desk is nearly always in the same zone.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use ferrite_config::{Config, ConfigError, Network, Notify, NotifyEvent, NotifyFormat, Path as At, Problem, Run};
use ferrite_config::{Secret, Security, Time, Wifi};
use snafu::{ResultExt, Snafu};

#[derive(Debug, Clone)]
pub struct SettingsOptions {
    /// The `.env` file to convert; `None` writes the template.
    pub env: Option<PathBuf>,
    /// Where to write the file; `None` prints it.
    pub out: Option<PathBuf>,
}

#[derive(Debug, Snafu)]
pub enum SettingsError {
    #[snafu(display("cannot read {}", path.display()))]
    Read { path: PathBuf, source: io::Error },

    #[snafu(display("{} is not valid UTF-8", path.display()))]
    NotText { path: PathBuf },

    #[snafu(display("{}: {source}", path.display()))]
    Env { path: PathBuf, source: EnvError },

    #[snafu(display("cannot write {}", path.display()))]
    Write { path: PathBuf, source: io::Error },
}

/// What is wrong with the `.env` lines, by key and line and never by value.
#[derive(Debug, Clone, PartialEq, Eq, Snafu)]
pub enum EnvError {
    #[snafu(display("line {line}: {key} does not end in a network number that fits"))]
    Number { key: String, line: usize },
    #[snafu(display("{key} is missing, though line {line} configures the same network"))]
    MissingSsid { key: String, line: usize },
    #[snafu(display("line {line}: {key} is not one of NONE, WPA-PSK, SAE"))]
    KeyMgmt { key: String, line: usize },
    #[snafu(display("line {line}: {key} is not 0, 1, true or false"))]
    Hidden { key: String, line: usize },
    #[snafu(display("line {line}: {key} needs a {passphrase_key} line, or WPA_KEY_MGMT=NONE for an open network"))]
    NeedsPassphrase { key: String, passphrase_key: String, line: usize },
    #[snafu(display("line {line}: {key} is not a ferrite setting"))]
    UnknownKey { key: String, line: usize },
    #[snafu(display("line {line}: {key} is not text or json"))]
    Format { key: String, line: usize },
    #[snafu(display("line {line}: {key} is not a comma-separated list of pass, fault and end"))]
    Events { key: String, line: usize },
    #[snafu(display("line {line}: {key} is not a whole number"))]
    Passes { key: String, line: usize },
    #[snafu(display("line {line}: {key} is not 1 or 0 (or yes, no, true, false, on, off)"))]
    Flag { key: String, line: usize },
    #[snafu(display("line {line}: {key} is set, but there is no NOTIFY_URL line"))]
    NoUrl { key: String, line: usize },
    #[snafu(display("line {line}: {key}: {problem}"))]
    Refused { key: String, line: usize, problem: Problem },
    #[snafu(display("{place}: {problem}"))]
    Elsewhere { place: String, problem: Problem },
}

/// Write the settings file the options describe.
pub fn run(options: &SettingsOptions) -> Result<(), SettingsError> {
    let text = match &options.env {
        None => Config::template(),
        Some(path) => {
            let bytes = fs::read(path).context(ReadSnafu { path })?;
            let env = String::from_utf8(bytes).map_err(|_| SettingsError::NotText { path: path.clone() })?;
            from_env(&env, host_zone().as_deref()).context(EnvSnafu { path })?
        },
    };
    match &options.out {
        Some(path) => {
            if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
                fs::create_dir_all(dir).context(WriteSnafu { path: dir })?;
            }
            fs::write(path, text).context(WriteSnafu { path })
        },
        None => io::stdout().write_all(text.as_bytes()).context(WriteSnafu { path: Path::new("standard output") }),
    }
}

/// The IANA name of this host's time zone, when it can be told and the apps' tz database knows it.
pub fn host_zone() -> Option<String> {
    let link = fs::read_link("/etc/localtime").ok();
    let file = fs::read_to_string("/etc/timezone").ok();
    zone_name(link.as_deref(), file.as_deref())
}

/// The zone a host's files name: the part of the `/etc/localtime` symlink's target under a `zoneinfo`
/// directory, else the first line of `/etc/timezone`. Only a name the bundled tz database knows counts.
pub fn zone_name(localtime_target: Option<&Path>, timezone_file: Option<&str>) -> Option<String> {
    let linked = localtime_target.and_then(|target| {
        let parts: Vec<_> = target.components().map(|part| part.as_os_str().to_str()).collect::<Option<_>>()?;
        let under = parts.iter().rposition(|part| *part == "zoneinfo")?;
        Some(parts[under + 1..].join("/"))
    });
    let named = timezone_file.and_then(|text| text.lines().next()).map(|line| line.trim().to_string());
    linked.into_iter().chain(named).find(|name| ferrite_config::zone_named(name).is_some())
}

/// The settings file for the `WPA_`, `NOTIFY_` and `RUN_` lines of `.env` text, checked the way the
/// apps will check it, with `[time]` in `zone` when there is one.
pub fn from_env(text: &str, zone: Option<&str>) -> Result<String, EnvError> {
    let mut converted = convert(text)?;
    converted.config.time = Time { zone: zone.map(str::to_string), ..Time::default() };
    let rendered = converted.config.render();
    let loaded = ferrite_config::load(rendered.as_bytes());
    if let Some(error) = loaded.errors.first() {
        return Err(converted.explain(error));
    }
    Ok(rendered)
}

/// A `KEY=VALUE` line.
#[derive(Clone, Copy)]
struct Line<'a> {
    number: usize,
    key: &'a str,
    value: &'a str,
}

#[derive(Default)]
struct Slot<'a> {
    ssid: Option<Line<'a>>,
    passphrase: Option<Line<'a>>,
    key_mgmt: Option<Line<'a>>,
    hidden: Option<Line<'a>>,
}

/// Where a place in the settings came from.
struct Origin {
    at: At,
    key: String,
    line: usize,
}

struct Converted {
    config: Config,
    origins: Vec<Origin>,
}

impl Converted {
    /// The error in `.env` terms: the key and line that produced the place it is at.
    fn explain(&self, error: &ConfigError) -> EnvError {
        let (error_at, problem) = (error.path.segments(), error.problem.clone());
        let origin = self
            .origins
            .iter()
            .filter(|o| error_at.starts_with(o.at.segments()))
            .max_by_key(|o| o.at.segments().len())
            .or_else(|| self.origins.iter().find(|o| o.at.segments().starts_with(error_at)));
        let Some(origin) = origin else {
            return EnvError::Elsewhere { place: error.path.to_string(), problem };
        };
        let (key, line) = (origin.key.clone(), origin.line);
        if problem == Problem::PassphraseMissing {
            let passphrase_key = key.replacen("WPA_SSID", "WPA_PASSPHRASE", 1);
            return EnvError::NeedsPassphrase { key, passphrase_key, line };
        }
        EnvError::Refused { key, line, problem }
    }
}

#[derive(Clone, Copy)]
enum Field {
    Ssid,
    Passphrase,
    KeyMgmt,
    Hidden,
}

impl<'a> Slot<'a> {
    fn set(&mut self, field: Field, line: Line<'a>) {
        let place = match field {
            Field::Ssid => &mut self.ssid,
            Field::Passphrase => &mut self.passphrase,
            Field::KeyMgmt => &mut self.key_mgmt,
            Field::Hidden => &mut self.hidden,
        };
        *place = Some(line);
    }
}

/// The setting and network number a `WPA_` key names: `WPA_SSID` is the first network, `WPA_SSID_3`
/// the third. `Ok(None)` for a key that is not a network setting, `Err` for a number that does not fit.
fn wpa_slot(key: &str) -> Result<Option<(Field, Option<u32>)>, ()> {
    const FIELDS: [(&str, Field); 4] = [
        ("WPA_SSID", Field::Ssid),
        ("WPA_PASSPHRASE", Field::Passphrase),
        ("WPA_KEY_MGMT", Field::KeyMgmt),
        ("WPA_HIDDEN", Field::Hidden),
    ];
    for (name, field) in FIELDS {
        let Some(rest) = key.strip_prefix(name) else { continue };
        if rest.is_empty() {
            return Ok(Some((field, None)));
        }
        let Some(digits) = rest.strip_prefix('_') else { continue };
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            return digits.parse().map(|n| Some((field, Some(n)))).map_err(|_| ());
        }
    }
    Ok(None)
}

fn unquote(value: &str) -> &str {
    value.strip_prefix('"').and_then(|rest| rest.strip_suffix('"')).unwrap_or(value)
}

fn flag(value: &str) -> Option<bool> {
    let is = |words: [&str; 4]| words.iter().any(|word| word.eq_ignore_ascii_case(value));
    if is(["1", "true", "yes", "on"]) {
        Some(true)
    } else if is(["0", "false", "no", "off"]) {
        Some(false)
    } else {
        None
    }
}

fn convert(text: &str) -> Result<Converted, EnvError> {
    let mut slots: BTreeMap<Option<u32>, Slot<'_>> = BTreeMap::new();
    let mut notify: BTreeMap<&str, Line<'_>> = BTreeMap::new();
    let mut run: BTreeMap<&str, Line<'_>> = BTreeMap::new();
    for (index, raw) in text.lines().enumerate() {
        let number = index + 1;
        let raw = raw.trim_end_matches('\r');
        let Some((key, value)) = raw.split_once('=') else { continue };
        if key.is_empty() || !key.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_') {
            continue;
        }
        if key.starts_with("WPA_") {
            let Ok(found) = wpa_slot(key) else { return Err(EnvError::Number { key: key.into(), line: number }) };
            if let Some((field, slot)) = found {
                slots.entry(slot).or_default().set(field, Line { number, key, value: unquote(value) });
            }
        } else if key.starts_with("NOTIFY_") || key.starts_with("RUN_") {
            let known: &[&str] = if key.starts_with("NOTIFY_") {
                &["NOTIFY_URL", "NOTIFY_FORMAT", "NOTIFY_EVENTS"]
            } else {
                &["RUN_PASSES", "RUN_TIME", "RUN_STOP_ON_FAULT"]
            };
            if !known.contains(&key) {
                return Err(EnvError::UnknownKey { key: key.into(), line: number });
            }
            let value = unquote(value.trim());
            let into = if key.starts_with("NOTIFY_") { &mut notify } else { &mut run };
            into.insert(key, Line { number, key, value });
        }
    }

    let mut origins = Vec::new();
    let mut networks = Vec::new();
    for slot in slots.into_values() {
        let at = At::default().key("wifi").key("networks").index(networks.len());
        networks.push(network(&slot, &at, &mut origins)?);
    }
    let notify = notify_section(&notify, &mut origins)?;
    let run = run_section(&run, &mut origins)?;
    Ok(Converted { config: Config { wifi: Wifi { networks }, notify, run, time: Time::default() }, origins })
}

fn origin(origins: &mut Vec<Origin>, at: At, line: Line<'_>) {
    origins.push(Origin { at, key: line.key.into(), line: line.number });
}

fn network(slot: &Slot<'_>, at: &At, origins: &mut Vec<Origin>) -> Result<Network, EnvError> {
    let Some(ssid) = slot.ssid else {
        let first = [slot.passphrase, slot.key_mgmt, slot.hidden].into_iter().flatten().min_by_key(|l| l.number);
        let (key, line) = first.map_or(("WPA_SSID", 0), |l| (l.key, l.number));
        let suffix = key.strip_prefix("WPA_PASSPHRASE").or_else(|| key.strip_prefix("WPA_KEY_MGMT"));
        let suffix = suffix.or_else(|| key.strip_prefix("WPA_HIDDEN")).unwrap_or_default();
        return Err(EnvError::MissingSsid { key: format!("WPA_SSID{suffix}"), line });
    };
    origin(origins, at.clone().key("ssid"), ssid);
    let security = match slot.key_mgmt {
        None => Security::Auto,
        Some(line) => {
            origin(origins, at.clone().key("security"), line);
            match line.value {
                "WPA-PSK" => Security::Wpa2,
                "SAE" => Security::Wpa3,
                "NONE" => Security::Open,
                _ => return Err(EnvError::KeyMgmt { key: line.key.into(), line: line.number }),
            }
        },
    };
    let hidden = match slot.hidden {
        None => false,
        Some(line) => {
            origin(origins, at.clone().key("hidden"), line);
            match line.value {
                "0" | "false" => false,
                "1" | "true" => true,
                _ => return Err(EnvError::Hidden { key: line.key.into(), line: line.number }),
            }
        },
    };
    let passphrase = slot.passphrase.map(|line| {
        origin(origins, at.clone().key("passphrase"), line);
        Secret::new(line.value)
    });
    Ok(Network { ssid: ssid.value.into(), passphrase, security, hidden })
}

fn notify_section(lines: &BTreeMap<&str, Line<'_>>, origins: &mut Vec<Origin>) -> Result<Option<Notify>, EnvError> {
    let at = At::default().key("notify");
    let Some(url) = lines.get("NOTIFY_URL") else {
        let first = lines.values().next();
        return first.map_or(Ok(None), |line| Err(EnvError::NoUrl { key: line.key.into(), line: line.number }));
    };
    origin(origins, at.clone().key("url"), *url);
    let format = match lines.get("NOTIFY_FORMAT") {
        None => NotifyFormat::default(),
        Some(line) => {
            origin(origins, at.clone().key("format"), *line);
            match line.value.to_ascii_lowercase().as_str() {
                "text" => NotifyFormat::Text,
                "json" => NotifyFormat::Json,
                _ => return Err(EnvError::Format { key: line.key.into(), line: line.number }),
            }
        },
    };
    let events = lines
        .get("NOTIFY_EVENTS")
        .map(|line| {
            origin(origins, at.clone().key("events"), *line);
            line.value
                .split(',')
                .map(|name| {
                    let name = name.trim();
                    NotifyEvent::ALL.into_iter().find(|event| event.name().eq_ignore_ascii_case(name))
                })
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| EnvError::Events { key: line.key.into(), line: line.number })
        })
        .transpose()?;
    Ok(Some(Notify { url: Secret::new(url.value), format, events }))
}

fn run_section(lines: &BTreeMap<&str, Line<'_>>, origins: &mut Vec<Origin>) -> Result<Run, EnvError> {
    let at = At::default().key("run");
    let mut run = Run::default();
    if let Some(line) = lines.get("RUN_PASSES") {
        origin(origins, at.clone().key("passes"), *line);
        let n = line.value.parse().map_err(|_| EnvError::Passes { key: line.key.into(), line: line.number })?;
        run.passes = Some(n);
    }
    if let Some(line) = lines.get("RUN_TIME") {
        origin(origins, at.clone().key("time"), *line);
        run.time = Some(line.value.into());
    }
    if let Some(line) = lines.get("RUN_STOP_ON_FAULT") {
        origin(origins, at.key("stop_on_fault"), *line);
        run.stop_on_fault =
            flag(line.value).ok_or_else(|| EnvError::Flag { key: line.key.into(), line: line.number })?;
    }
    Ok(run)
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::time::Duration;

    use assert2::check;
    use ferrite_config::load;
    use rstest::rstest;

    use super::*;

    const ENV: &str = "\
# the developer's credentials
CARGO_TERM_COLOR=always
WPA_SSID=\"Home Net\"
WPA_PASSPHRASE=\"a\\\"b # c d\"
WPA_SSID_10=ten
WPA_KEY_MGMT_10=NONE
WPA_SSID_2=two
WPA_PASSPHRASE_2=passphrase2
WPA_KEY_MGMT_2=SAE
WPA_HIDDEN_2=true
WPA_SSID_01=one
WPA_PASSPHRASE_01=passphrase1
WPA_KEY_MGMT_01=WPA-PSK
WPA_FUTURE=ignored
NOTIFY_URL=http://ntfy.sh/topic
NOTIFY_FORMAT=JSON
NOTIFY_EVENTS=pass, END
RUN_PASSES=3
RUN_TIME=1h30m
RUN_STOP_ON_FAULT=yes
WPA_SSID_01=last-one-wins\r
";

    fn network(ssid: &str, passphrase: Option<&str>, security: Security, hidden: bool) -> Network {
        Network { ssid: ssid.into(), passphrase: passphrase.map(Secret::new), security, hidden }
    }

    #[test]
    fn env_lines_become_the_settings_they_always_meant() {
        let config = convert(ENV).unwrap().config;
        let want = Config {
            wifi: Wifi {
                networks: vec![
                    network("Home Net", Some("a\\\"b # c d"), Security::Auto, false),
                    network("last-one-wins", Some("passphrase1"), Security::Wpa2, false),
                    network("two", Some("passphrase2"), Security::Wpa3, true),
                    network("ten", None, Security::Open, false),
                ],
            },
            notify: Some(Notify {
                url: Secret::new("http://ntfy.sh/topic"),
                format: NotifyFormat::Json,
                events: Some(vec![NotifyEvent::Pass, NotifyEvent::End]),
            }),
            run: Run { passes: Some(3), time: Some("1h30m".into()), stop_on_fault: true },
            time: Time::default(),
        };
        check!(config == want);
    }

    #[test]
    fn the_file_written_from_env_is_accepted_by_the_apps_checks() {
        let text = from_env(ENV, None).unwrap();
        let loaded = load(text.as_bytes());
        check!(loaded.errors.is_empty(), "{:?}", loaded.errors);
        check!(loaded.credentials.is_some_and(|c| c.networks().len() == 4));
        check!(loaded.notify.is_some());
        check!(loaded.run.is_some_and(|goal| goal.time == Some(Duration::from_mins(90))));
    }

    #[rstest]
    #[case::nothing("")]
    #[case::unrelated("PATH=/bin\nFOO_BAR=1\n# WPA_SSID=x\n")]
    fn an_env_without_settings_gives_the_template(#[case] env: &str) {
        let text = from_env(env, None).unwrap();
        check!(text == Config::template());
        check!(load(text.as_bytes()).credentials.is_none());
    }

    const SECRET: &str = "s3cr3t-value";

    #[rstest]
    #[case::bad_number("WPA_SSID_99999999999=x\n".into(), "line 1: WPA_SSID_99999999999 does not end in a network number that fits")]
    #[case::no_ssid(
        "WPA_SSID=a\nWPA_KEY_MGMT=NONE\nWPA_PASSPHRASE_2=12345678\n".into(),
        "WPA_SSID_2 is missing, though line 3 configures the same network"
    )]
    #[case::key_mgmt(format!("WPA_SSID=x\nWPA_KEY_MGMT={SECRET}\n"), "line 2: WPA_KEY_MGMT is not one of NONE, WPA-PSK, SAE")]
    #[case::hidden(format!("WPA_SSID=x\nWPA_HIDDEN_1={SECRET}\nWPA_SSID_1=y\n"), "line 2: WPA_HIDDEN_1 is not 0, 1, true or false")]
    #[case::needs_passphrase(
        "WPA_SSID=a\nWPA_PASSPHRASE=12345678\nWPA_SSID_3=x\n".into(),
        "line 3: WPA_SSID_3 needs a WPA_PASSPHRASE_3 line, or WPA_KEY_MGMT=NONE for an open network"
    )]
    #[case::short_passphrase(
        "WPA_SSID=x\nWPA_PASSPHRASE=s3cr3t\n".into(),
        "line 2: WPA_PASSPHRASE: a passphrase is 8 to 63 printable ASCII characters"
    )]
    #[case::open_with_passphrase(
        format!("WPA_SSID=x\nWPA_KEY_MGMT=NONE\nWPA_PASSPHRASE={SECRET}\n"),
        "line 3: WPA_PASSPHRASE: an open network takes no passphrase"
    )]
    #[case::long_ssid(
        format!("WPA_SSID={}\nWPA_KEY_MGMT=NONE\n", "a".repeat(40)),
        "line 1: WPA_SSID: an SSID is 1 to 32 bytes"
    )]
    #[case::unknown_notify(format!("NOTIFY_TOPIC={SECRET}\n"), "line 1: NOTIFY_TOPIC is not a ferrite setting")]
    #[case::unknown_run(format!("RUN_FOREVER={SECRET}\n"), "line 1: RUN_FOREVER is not a ferrite setting")]
    #[case::https(
        format!("NOTIFY_URL=https://ntfy.sh/{SECRET}\n"),
        "line 1: NOTIFY_URL: https is not supported (no TLS), use an http:// URL"
    )]
    #[case::format(format!("NOTIFY_URL=http://h/t\nNOTIFY_FORMAT={SECRET}\n"), "line 2: NOTIFY_FORMAT is not text or json")]
    #[case::events(
        format!("NOTIFY_URL=http://h/t\nNOTIFY_EVENTS=pass,{SECRET}\n"),
        "line 2: NOTIFY_EVENTS is not a comma-separated list of pass, fault and end"
    )]
    #[case::no_url("NOTIFY_FORMAT=json\n".into(), "line 1: NOTIFY_FORMAT is set, but there is no NOTIFY_URL line")]
    #[case::passes(format!("RUN_PASSES={SECRET}\n"), "line 1: RUN_PASSES is not a whole number")]
    #[case::zero_passes("RUN_PASSES=0\n".into(), "line 1: RUN_PASSES: expected a whole number of passes, at least 1")]
    #[case::time(
        format!("RUN_TIME={SECRET}\n"),
        "line 1: RUN_TIME: expected a time such as 90m, 2h or 1h30m, between 1 second and a year"
    )]
    #[case::flag(format!("RUN_STOP_ON_FAULT={SECRET}\n"), "line 1: RUN_STOP_ON_FAULT is not 1 or 0 (or yes, no, true, false, on, off)")]
    fn a_broken_env_is_refused_by_key_and_line_and_never_by_value(#[case] env: String, #[case] message: &str) {
        let error = from_env(&env, None).unwrap_err();
        check!(error.to_string() == message);
        let shown = format!("{error} {error:?}");
        check!(!shown.contains(SECRET) && !shown.contains("s3cr3t"), "{shown}");
    }

    #[test]
    fn the_hosts_zone_is_written_into_the_file_and_read_back_by_the_apps_checks() {
        let text = from_env("RUN_PASSES=2\n", Some("America/Chicago")).unwrap();
        check!(text.contains("\n[time]\n"), "{text}");
        check!(text.contains("\nzone = \"America/Chicago\"\n"), "{text}");
        let time = load(text.as_bytes()).time.unwrap();
        check!(time.zone.iana_name() == Some("America/Chicago"));
        check!(time.ntp, "the other keys stay at their defaults, commented");
    }

    #[rstest]
    #[case::debian_link(Some("/usr/share/zoneinfo/America/Chicago"), None, Some("America/Chicago"))]
    #[case::relative_link(Some("../usr/share/zoneinfo/Europe/Berlin"), None, Some("Europe/Berlin"))]
    #[case::mac_link(Some("/var/db/timezone/zoneinfo/Asia/Tokyo"), None, Some("Asia/Tokyo"))]
    #[case::one_part(Some("/usr/share/zoneinfo/UTC"), None, Some("UTC"))]
    #[case::the_link_wins(Some("/usr/share/zoneinfo/Europe/Paris"), Some("Asia/Tokyo\n"), Some("Europe/Paris"))]
    #[case::a_copy_not_a_link(None, Some("America/New_York\n"), Some("America/New_York"))]
    #[case::a_link_elsewhere(Some("/etc/mytz"), Some("America/Denver"), Some("America/Denver"))]
    #[case::a_name_the_database_lacks(Some("/usr/share/zoneinfo/posix/America/Chicago"), None, None)]
    #[case::a_name_the_database_lacks_then_the_file(
        Some("/usr/share/zoneinfo/right/UTC"),
        Some("Etc/UTC\n"),
        Some("Etc/UTC")
    )]
    #[case::garbage(Some("/dev/null"), Some("Local\n"), None)]
    #[case::nothing(None, None, None)]
    fn the_hosts_zone_is_the_name_its_files_give(
        #[case] link: Option<&str>,
        #[case] file: Option<&str>,
        #[case] want: Option<&str>,
    ) {
        check!(zone_name(link.map(Path::new), file).as_deref() == want);
    }

    #[test]
    fn too_many_networks_name_the_ssid_line_of_the_first_one_over() {
        let max = ferrite_config::MAX_NETWORKS;
        let mut env = String::new();
        for n in 0..=max {
            let _ = writeln!(env, "WPA_SSID_{n}=n{n}\nWPA_KEY_MGMT_{n}=NONE");
        }
        let error = from_env(&env, None).unwrap_err();
        check!(error.to_string() == format!("line {}: WPA_SSID_{max}: at most {max} networks", max * 2 + 1));
    }

    #[test]
    fn the_file_is_written_where_asked_with_its_directory() {
        let dir = std::env::temp_dir().join(format!("ferrite-dev-settings-{}", std::process::id()));
        let env = dir.join("env");
        let out = dir.join("esp/efi/ferrite/ferrite.toml");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&env, "WPA_SSID=x\nWPA_KEY_MGMT=NONE\n").unwrap();
        run(&SettingsOptions { env: Some(env), out: Some(out.clone()) }).unwrap();
        let text = fs::read_to_string(&out).unwrap();
        check!(load(text.as_bytes()).credentials.is_some());
        fs::remove_dir_all(&dir).unwrap();
    }
}
