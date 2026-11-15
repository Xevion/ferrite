use alloc::format;
use alloc::string::{String, ToString as _};
use alloc::vec::Vec;
use core::time::Duration;

use assert2::{assert, check};
use ferrite_net::notify::{Event, Events, Format, Host, HostName};
use ferrite_wpa::env::{Allow, Auth};
use rstest::rstest;

use super::*;

const FULL: &str = r#"
[[wifi.networks]]
ssid = "Home"
passphrase = "correct horse battery"

[[wifi.networks]]
ssid = "Cafe guest"
security = "open"

[[wifi.networks]]
ssid = "lab-ap"
passphrase = "another passphrase"
security = "wpa3"
hidden = true

[[wifi.networks]]
ssid = "old-router"
passphrase = "twelve chars!"
security = "wpa2"

[notify]
url = "http://ntfy.local:8080/ferrite-topic"
format = "json"
events = ["pass", "end"]

[run]
passes = 3
time = "1h30m"
stop_on_fault = true

[time]
ntp = false
servers = ["192.0.2.1", "time.example.org"]
zone = "America/Chicago"
clock = "24h"
"#;

#[test]
fn a_file_with_every_section_is_read_in_file_order() {
    let loaded = load(FULL.as_bytes());
    check!(loaded.errors.is_empty());

    assert!(let Some(credentials) = &loaded.credentials);
    let networks = credentials.networks();
    check!(
        networks.iter().map(|n| n.ssid.as_bytes()).collect::<Vec<_>>()
            == [&b"Home"[..], b"Cafe guest", b"lab-ap", b"old-router"]
    );
    check!(networks.iter().map(|n| n.hidden).collect::<Vec<_>>() == [false, false, true, false]);
    check!(networks[1].auth == Auth::Open);
    let allowed: Vec<Allow> = networks
        .iter()
        .filter_map(|n| match &n.auth {
            Auth::Personal { allow, .. } => Some(*allow),
            Auth::Open => None,
        })
        .collect();
    check!(allowed == [Allow::Any, Allow::SaeOnly, Allow::PskOnly]);

    assert!(let Some(notify) = &loaded.notify);
    check!(notify.endpoint.host == Host::Name(HostName::new("ntfy.local").unwrap()));
    check!(notify.endpoint.port == 8080);
    check!(notify.endpoint.target == "/ferrite-topic");
    check!(notify.format == Format::Json);
    check!(notify.events == Events::of(&[Event::Pass, Event::End]));

    check!(
        loaded.run
            == Some(RunGoal {
                passes: core::num::NonZeroU64::new(3),
                time: Some(Duration::from_mins(90)),
                stop_on_fault: true,
            })
    );

    assert!(let Some(time) = &loaded.time);
    check!(!time.ntp);
    check!(time.servers == [Host::parse("192.0.2.1").unwrap(), Host::Name(HostName::new("time.example.org").unwrap())]);
    check!(time.zone.iana_name() == Some("America/Chicago"));
    check!(time.clock == ClockFormat::TwentyFour);
}

#[rstest]
#[case::empty("")]
#[case::only_comments("# nothing here\n")]
fn a_file_that_configures_nothing_is_not_an_error(#[case] text: &str) {
    let loaded = load(text.as_bytes());
    check!(loaded.errors.is_empty());
    check!(loaded.credentials.is_none() && loaded.notify.is_none() && loaded.run.is_none() && loaded.time.is_none());
}

#[test]
fn defaults_apply_to_what_a_section_leaves_out() {
    let loaded = load(
        b"[notify]\nurl = \"http://h/t\"\n[run]\n[time]\n[[wifi.networks]]\nssid = \"x\"\npassphrase = \"12345678\"\n",
    );
    check!(loaded.errors.is_empty());
    let notify = loaded.notify.unwrap();
    check!(notify.format == Format::Text && notify.events == Events::ALL);
    check!(loaded.run == Some(RunGoal::default()));
    check!(loaded.time == Some(TimeSettings::default()));
    check!(TimeSettings::default().ntp, "the clock is kept right over the network unless the file says no");
    let credentials = loaded.credentials.unwrap();
    let network = &credentials.networks()[0];
    check!(!network.hidden);
    assert!(let Auth::Personal { allow: Allow::Any, .. } = &network.auth);
}

/// The value that must never show up in a message.
const SECRET: &str = "s3cr3t-value";

fn failing(text: &str) -> ConfigError {
    let mut errors = load(text.as_bytes()).errors;
    assert!(errors.len() == 1, "{errors:?}");
    errors.remove(0)
}

fn at(path: &str) -> Path {
    if path.is_empty() {
        return Path::default();
    }
    path.split('.').fold(Path::default(), |path, part| match part.split_once('[') {
        Some((key, index)) => path.key(key).index(index.trim_end_matches(']').parse().unwrap()),
        None => path.key(part),
    })
}

#[rstest]
#[case::syntax_error(
    format!("[run]\npasses = {SECRET}\n"),
    2, 10, "", None
)]
#[case::unknown_section(
    format!("[wifii]\nkey = \"{SECRET}\"\n"),
    1, 2, "wifii", Some(Problem::UnknownKey)
)]
#[case::unknown_key(
    format!("[[wifi.networks]]\nssid = \"x\"\npassphrse = \"{SECRET}\"\n"),
    3, 1, "wifi.networks[0].passphrse", Some(Problem::UnknownKey)
)]
#[case::missing_ssid(
    format!("[[wifi.networks]]\npassphrase = \"{SECRET}\"\n"),
    1, 1, "wifi.networks[0]", Some(Problem::MissingKey { key: "ssid".into() })
)]
#[case::missing_url(
    "[notify]\nformat = \"json\"\n".into(),
    1, 1, "notify", Some(Problem::MissingKey { key: "url".into() })
)]
#[case::passphrase_type(
    format!("[[wifi.networks]]\nssid = \"x\"\npassphrase = [\"{SECRET}\"]\n"),
    3, 14, "wifi.networks[0].passphrase", Some(Problem::WrongType { expected: "a string".into() })
)]
#[case::hidden_type(
    format!("[[wifi.networks]]\nssid = \"x\"\nsecurity = \"open\"\nhidden = \"{SECRET}\"\n"),
    4, 10, "wifi.networks[0].hidden", Some(Problem::WrongType { expected: "a boolean".into() })
)]
#[case::networks_type(
    format!("[wifi]\nnetworks = \"{SECRET}\"\n"),
    2, 12, "wifi.networks", Some(Problem::WrongType { expected: "an array".into() })
)]
#[case::section_type(
    format!("notify = \"{SECRET}\"\n"),
    1, 10, "notify", Some(Problem::WrongType { expected: "a table".into() })
)]
#[case::security_variant(
    format!("[[wifi.networks]]\nssid = \"x\"\nsecurity = \"{SECRET}\"\n"),
    3, 12, "wifi.networks[0].security",
    Some(Problem::BadValue { expected: "one of `auto`, `open`, `wpa2`, `wpa3`".into() })
)]
#[case::format_variant(
    format!("[notify]\nurl = \"http://h/t\"\nformat = \"{SECRET}\"\n"),
    3, 10, "notify.format", Some(Problem::BadValue { expected: "`text` or `json`".into() })
)]
#[case::event_variant(
    format!("[notify]\nurl = \"http://h/t\"\nevents = [\"pass\", \"{SECRET}\"]\n"),
    3, 19, "notify.events[1]", Some(Problem::BadValue { expected: "one of `pass`, `fault`, `end`".into() })
)]
#[case::passes_negative(
    "[run]\npasses = -4242\n".into(),
    2, 10, "run.passes", Some(Problem::BadValue { expected: "u64".into() })
)]
#[case::passes_zero("[run]\npasses = 0\n".into(), 2, 10, "run.passes", Some(Problem::Passes))]
#[case::time_unit(format!("[run]\ntime = \"{SECRET}\"\n"), 2, 8, "run.time", Some(Problem::Time))]
#[case::time_year("[run]\ntime = \"400d\"\n".into(), 2, 8, "run.time", Some(Problem::Time))]
#[case::ssid_empty(
    "[[wifi.networks]]\nssid = \"\"\nsecurity = \"open\"\n".into(),
    2, 8, "wifi.networks[0].ssid", Some(Problem::Ssid)
)]
#[case::ssid_long(
    format!("[[wifi.networks]]\nssid = \"{}\"\nsecurity = \"open\"\n", "a".repeat(33)),
    2, 8, "wifi.networks[0].ssid", Some(Problem::Ssid)
)]
#[case::passphrase_short(
    "[[wifi.networks]]\nssid = \"x\"\npassphrase = \"s3cr3t\"\n".into(),
    3, 14, "wifi.networks[0].passphrase", Some(Problem::Passphrase)
)]
#[case::passphrase_non_ascii(
    "[[wifi.networks]]\nssid = \"x\"\npassphrase = \"s3cr3t-vaer\u{e9}\"\n".into(),
    3, 14, "wifi.networks[0].passphrase", Some(Problem::Passphrase)
)]
#[case::passphrase_long(
    format!("[[wifi.networks]]\nssid = \"x\"\npassphrase = \"{}\"\n", "p".repeat(64)),
    3, 14, "wifi.networks[0].passphrase", Some(Problem::Passphrase)
)]
#[case::needs_a_passphrase(
    "[[wifi.networks]]\nssid = \"a\"\npassphrase = \"12345678\"\n[[wifi.networks]]\nssid = \"b\"\n".into(),
    5, 8, "wifi.networks[1].ssid", Some(Problem::PassphraseMissing)
)]
#[case::open_with_passphrase(
    format!("[[wifi.networks]]\nssid = \"x\"\nsecurity = \"open\"\npassphrase = \"{SECRET}\"\n"),
    4, 14, "wifi.networks[0].passphrase", Some(Problem::OpenWithPassphrase)
)]
#[case::no_networks("[wifi]\n".into(), 1, 1, "wifi", Some(Problem::NoNetworks))]
#[case::url_https(
    format!("[notify]\nurl = \"https://ntfy.sh/{SECRET}\"\n"),
    2, 7, "notify.url", Some(Problem::Url { problem: ferrite_net::notify::Problem::Https })
)]
#[case::url_credentials(
    format!("[notify]\nurl = \"http://user:{SECRET}@host/t\"\n"),
    2, 7, "notify.url", Some(Problem::Url { problem: ferrite_net::notify::Problem::UserInfo })
)]
#[case::url_port(
    format!("[notify]\nurl = \"http://host:0/{SECRET}\"\n"),
    2, 7, "notify.url", Some(Problem::Url { problem: ferrite_net::notify::Problem::Port })
)]
#[case::no_events("[notify]\nurl = \"http://h/t\"\nevents = []\n".into(), 3, 10, "notify.events", Some(Problem::NoEvents))]
#[case::unknown_zone(format!("[time]\nzone = \"Mars/{SECRET}\"\n"), 2, 8, "time.zone", Some(Problem::Zone))]
#[case::unknown_zone_spelling("[time]\nzone = \"Chicago\"\n".into(), 2, 8, "time.zone", Some(Problem::Zone))]
#[case::zone_type("[time]\nzone = 5\n".into(), 2, 8, "time.zone", Some(Problem::WrongType { expected: "a string".into() }))]
#[case::ntp_type(format!("[time]\nntp = \"{SECRET}\"\n"), 2, 7, "time.ntp", Some(Problem::WrongType { expected: "a boolean".into() }))]
#[case::clock_variant(
    format!("[time]\nclock = \"{SECRET}\"\n"),
    2, 9, "time.clock", Some(Problem::BadValue { expected: "`12h` or `24h`".into() })
)]
#[case::server_not_a_host(
    format!("[time]\nservers = [\"10.0.0.1\", \"bad host {SECRET}\"]\n"),
    2, 24, "time.servers[1]", Some(Problem::NtpServer)
)]
#[case::server_is_a_url(
    format!("[time]\nservers = [\"http://{SECRET}\"]\n"),
    2, 12, "time.servers[0]", Some(Problem::NtpServer)
)]
#[case::cpu_temp_range("[cpu]\nmax_temp = 120\n".into(), 2, 12, "cpu.max_temp", Some(Problem::TempLimit))]
#[case::cpu_tolerance_range("[cpu]\ntolerance = 60\n".into(), 2, 13, "cpu.tolerance", Some(Problem::Tolerance))]
#[case::cpu_clock_variant(
    format!("[cpu]\nclock = \"{SECRET}\"\n"),
    2, 9, "cpu.clock", Some(Problem::BadValue { expected: "one of `firmware`, `max`, `auto`, `calibrate`".into() })
)]
#[case::unknown_time_key(format!("[time]\nzones = \"{SECRET}\"\n"), 2, 1, "time.zones", Some(Problem::UnknownKey))]
fn a_bad_file_names_its_line_column_and_key_and_never_the_value(
    #[case] text: String,
    #[case] line: usize,
    #[case] column: usize,
    #[case] path: &str,
    #[case] problem: Option<Problem>,
) {
    let error = failing(&text);
    check!((error.line, error.column) == (line, column), "{error}");
    check!(error.path == at(path), "{error}");
    if let Some(problem) = problem {
        check!(error.problem == problem);
    } else {
        assert!(let Problem::Syntax { .. } = &error.problem);
    }
    let shown = format!("{error} {error:?}");
    check!(!shown.contains(SECRET) && !shown.contains("s3cr3t"), "{shown}");
}

#[test]
fn too_many_networks_are_refused_at_the_first_one_beyond_the_limit() {
    let text = "[[wifi.networks]]\nssid = \"x\"\nsecurity = \"open\"\n".repeat(MAX_NETWORKS + 1);
    let error = failing(&text);
    check!(error.problem == Problem::TooManyNetworks { max: MAX_NETWORKS });
    check!(error.path == at(&format!("wifi.networks[{MAX_NETWORKS}]")));
    check!(load(text.lines().take(MAX_NETWORKS * 3).collect::<Vec<_>>().join("\n").as_bytes()).errors.is_empty());
}

#[test]
fn too_many_ntp_servers_are_refused_at_the_first_beyond_the_limit() {
    let list = |n: usize| (0..n).map(|i| format!("\"10.0.0.{i}\"")).collect::<Vec<_>>().join(", ");
    let max = ferrite_net::sntp::MAX_SERVERS;
    let error = failing(&format!("[time]\nservers = [{}]\n", list(max + 1)));
    check!(error.problem == Problem::TooManyNtpServers { max });
    check!(error.path == at(&format!("time.servers[{max}]")));
    check!(load(format!("[time]\nservers = [{}]\n", list(max)).as_bytes()).errors.is_empty());
}

#[rstest]
#[case("UTC", "UTC")]
#[case("Etc/UTC", "Etc/UTC")]
#[case("america/new_york", "America/New_York")]
#[case("Asia/Kolkata", "Asia/Kolkata")]
fn a_zone_is_a_name_of_the_bundled_tz_database_in_any_case(#[case] given: &str, #[case] canonical: &str) {
    let loaded = load(format!("[time]\nzone = \"{given}\"\n").as_bytes());
    check!(loaded.errors.is_empty(), "{:?}", loaded.errors);
    check!(loaded.time.unwrap().zone.iana_name() == Some(canonical));
}

#[test]
fn the_zone_keeps_its_daylight_saving_rules() {
    use jiff::Timestamp;

    let loaded = load(b"[time]\nzone = \"America/Chicago\"\n");
    let zone = loaded.time.unwrap().zone;
    let offset = |ts: &str| zone.to_offset(ts.parse::<Timestamp>().unwrap()).seconds();
    check!(offset("2026-01-15T12:00:00Z") == -6 * 3600);
    check!(offset("2026-07-15T12:00:00Z") == -5 * 3600);
}

#[test]
fn the_message_reads_as_one_line_with_the_place_first() {
    let error = failing("[[wifi.networks]]\nssid = \"x\"\npassphrase = \"s3cr3t\"\n");
    check!(
        error.to_string()
            == "line 3, column 14: wifi.networks[0].passphrase: a passphrase is 8 to 63 printable ASCII characters"
    );
}

#[test]
fn a_file_that_is_not_text_is_refused_at_the_bad_byte() {
    let mut text = b"[notify]\nurl = \"http://h/".to_vec();
    text.push(0xff);
    let loaded = load(&text);
    assert!(let [error] = loaded.errors.as_slice());
    check!((error.line, error.column) == (2, 17));
    check!(error.problem == Problem::Encoding);
}

#[test]
fn a_leading_byte_order_mark_is_ignored() {
    let loaded = load("\u{feff}[run]\npasses = 2\n".as_bytes());
    check!(loaded.errors.is_empty());
    check!(loaded.run.is_some());
}

#[test]
fn columns_count_characters_not_bytes() {
    let before = "wifi = { networks = [{ ssid = \"\u{e9}\u{e9}\", passphrase = ";
    let error = failing(&format!("{before}\"s3cr3t\" }}] }}\n"));
    check!(error.path == at("wifi.networks[0].passphrase"));
    check!((error.line, error.column) == (1, before.chars().count() + 1));
}

#[test]
fn a_bad_section_leaves_the_others_in_force() {
    let text = "[[wifi.networks]]\nssid = \"x\"\npassphrase = \"12345678\"\n[notify]\nurl = \"https://h/t\"\n[run]\npasses = 2\n";
    let loaded = load(text.as_bytes());
    check!(loaded.credentials.is_some());
    check!(loaded.notify.is_none());
    check!(loaded.run.is_some());
    assert!(let [error] = loaded.errors.as_slice());
    check!(error.section() == Some("notify"));
}

#[test]
fn every_bad_section_is_reported() {
    let text = "[wifi]\n[notify]\nurl = \"ftp://h\"\n[run]\ntime = \"x\"\n[extra]\n";
    let errors = load(text.as_bytes()).errors;
    let sections: Vec<_> = errors.iter().map(ConfigError::section).collect();
    check!(sections.len() == 4);
    for section in ["wifi", "notify", "run", "extra"] {
        check!(sections.contains(&Some(section)), "{sections:?}");
    }
}

#[test]
fn a_file_that_is_not_toml_configures_nothing() {
    let loaded = load(b"[wifi\n");
    check!(loaded.errors.len() == 1);
    check!(loaded.credentials.is_none() && loaded.notify.is_none() && loaded.run.is_none());
    check!(loaded.errors[0].section().is_none());
}

#[test]
fn debug_output_hides_passphrases_and_the_url() {
    let text = "[[wifi.networks]]\nssid = \"x\"\npassphrase = \"s3cr3t-passphrase\"\n[notify]\nurl = \"http://h/s3cr3t-topic\"\n";
    let loaded = load(text.as_bytes());
    check!(loaded.errors.is_empty());
    let shown = format!("{loaded:?}");
    check!(!shown.contains("s3cr3t"), "{shown}");
    let plain = Config {
        wifi: Wifi {
            networks: alloc::vec![Network {
                ssid: "x".into(),
                passphrase: Some(Secret::new("s3cr3t-passphrase")),
                security: Security::Auto,
                hidden: false,
            }],
        },
        notify: Some(Notify { url: Secret::new("http://h/s3cr3t-topic"), format: NotifyFormat::Text, events: None }),
        run: Run::default(),
        time: Time::default(),
        cpu: Cpu::default(),
    };
    check!(!format!("{plain:?}").contains("s3cr3t"));
}

#[test]
fn into_result_gives_the_first_error_or_the_settings() {
    check!(load(FULL.as_bytes()).into_result().is_ok());
    assert!(let Err(error) = load(b"[run]\npasses = 0\n").into_result());
    check!(error.problem == Problem::Passes);
}

fn sample() -> Config {
    Config {
        wifi: Wifi {
            networks: alloc::vec![
                Network {
                    ssid: "My \"Home\" \\ Net \u{e9}".into(),
                    passphrase: Some(Secret::new("pa\"ss\\word # with 'quotes' and \u{7e}")),
                    security: Security::Wpa3,
                    hidden: true,
                },
                Network { ssid: "Cafe".into(), passphrase: None, security: Security::Open, hidden: false },
                Network {
                    ssid: "plain".into(),
                    passphrase: Some(Secret::new("12345678")),
                    security: Security::Auto,
                    hidden: false,
                },
            ],
        },
        notify: Some(Notify {
            url: Secret::new("http://10.0.0.5:8080/t?a=b"),
            format: NotifyFormat::Json,
            events: Some(alloc::vec![NotifyEvent::Fault, NotifyEvent::End]),
        }),
        run: Run { passes: Some(2), time: Some("45m".into()), stop_on_fault: true },
        time: Time {
            ntp: false,
            servers: Some(alloc::vec!["10.1.1.1".into(), "ntp.lab.example".into()]),
            zone: Some("Europe/Berlin".into()),
            clock: ClockFormat::TwentyFour,
        },
        cpu: Cpu { clock: CpuClock::Max, max_temp: Some(88), tolerance: Some(5) },
    }
}

#[test]
fn a_rendered_file_reads_back_as_the_configuration_it_came_from() {
    let config = sample();
    let text = config.render();
    let loaded = load(text.as_bytes());
    check!(loaded.errors.is_empty(), "{:?}\n{text}", loaded.errors);
    let credentials = loaded.credentials.unwrap();
    let networks = credentials.networks();
    check!(networks.len() == 3);
    check!(networks[0].ssid.as_bytes() == "My \"Home\" \\ Net \u{e9}".as_bytes());
    check!(networks[0].hidden);
    assert!(let Auth::Personal { allow: Allow::SaeOnly, passphrase } = &networks[0].auth);
    check!(passphrase.len() == "pa\"ss\\word # with 'quotes' and ~".len());
    check!(networks[1].auth == Auth::Open);
    let notify = loaded.notify.unwrap();
    check!(notify.format == Format::Json);
    check!(notify.events == Events::of(&[Event::Fault, Event::End]));
    check!(notify.endpoint.target == "/t?a=b");
    check!(loaded.run.unwrap().time == Some(Duration::from_mins(45)));
    let time = loaded.time.unwrap();
    check!(!time.ntp && time.clock == ClockFormat::TwentyFour);
    check!(time.zone.iana_name() == Some("Europe/Berlin"));
    check!(time.servers == [Host::parse("10.1.1.1").unwrap(), Host::Name(HostName::new("ntp.lab.example").unwrap())]);
}

#[test]
fn the_template_is_all_comments_and_reads_as_nothing_configured() {
    let template = Config::template();
    let loaded = load(template.as_bytes());
    check!(loaded.errors.is_empty(), "{:?}", loaded.errors);
    check!(loaded.credentials.is_none() && loaded.notify.is_none() && loaded.run.is_none() && loaded.time.is_none());
    check!(template.lines().all(|line| line.is_empty() || line.starts_with('#')));
    for key in [
        "ssid",
        "passphrase",
        "security",
        "hidden",
        "url",
        "format",
        "events",
        "passes",
        "time",
        "stop_on_fault",
        "ntp",
        "servers",
        "zone",
        "clock",
    ] {
        check!(template.contains(&format!("# {key} = ")) || template.contains(&format!("{key} = ")), "{key}");
    }
}

#[test]
fn uncommenting_every_key_and_header_of_the_template_gives_a_valid_file() {
    let is_key = |rest: &str| {
        rest.starts_with('[')
            || rest.split_once(" = ").is_some_and(|(key, _)| key.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
    };
    let uncommented = Config::template().lines().fold(String::new(), |mut text, line| {
        text.push_str(line.strip_prefix("# ").filter(|rest| is_key(rest)).unwrap_or(line));
        text.push('\n');
        text
    });
    let loaded = load(uncommented.as_bytes());
    check!(loaded.errors.is_empty(), "{:?}\n{uncommented}", loaded.errors);
    check!(loaded.credentials.is_some() && loaded.notify.is_some() && loaded.run.is_some() && loaded.time.is_some());
}

#[test]
fn defaults_stay_commented_when_values_are_filled_in() {
    let config = Config {
        wifi: Wifi {
            networks: alloc::vec![Network {
                ssid: "x".into(),
                passphrase: Some(Secret::new("12345678")),
                security: Security::Auto,
                hidden: false,
            }],
        },
        notify: Some(Notify { url: Secret::new("http://h/t"), format: NotifyFormat::Text, events: None }),
        run: Run::default(),
        time: Time::default(),
        cpu: Cpu::default(),
    };
    let text = config.render();
    for set in ["\nssid = ", "\npassphrase = ", "\nurl = ", "\n[[wifi.networks]]", "\n[notify]"] {
        check!(text.contains(set), "{set}");
    }
    for unset in [
        "\n# security = ",
        "\n# hidden = ",
        "\n# format = ",
        "\n# events = ",
        "\n# [run]",
        "\n# passes = ",
        "\n# [time]",
        "\n# ntp = ",
        "\n# zone = ",
        "\n# [cpu]",
        "\n# clock = ",
    ] {
        check!(text.contains(unset), "{unset}");
    }
}

#[test]
fn the_cpu_section_becomes_a_policy_and_defaults_to_auto() {
    let loaded = load(b"[cpu]\nclock = \"calibrate\"\nmax_temp = 85\n");
    check!(loaded.errors.is_empty(), "{:?}", loaded.errors);
    let policy = loaded.cpu.unwrap();
    check!(policy.clock == ferrite_power::policy::ClockMode::Calibrate);
    check!(policy.max_temp.map(ferrite_power::policy::TempLimit::celsius) == Some(85));
    check!(policy.tolerance == ferrite_power::policy::Tolerance::DEFAULT);
    check!(load(b"[cpu]\n").cpu == Some(ferrite_power::policy::Policy::default()));
}
