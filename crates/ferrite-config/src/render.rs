//! The file as text: a template with every key, its meaning and its default, and the same text with
//! the values of a [`Config`] filled in.

use alloc::string::{String, ToString as _};
use alloc::vec::Vec;
use core::fmt::Write as _;

use crate::MAX_NETWORKS;
use crate::schema::{Config, Network, Notify, NotifyEvent, NotifyFormat, Run, Secret, Security};

const HEADER: &str = "\
## ferrite settings, read from \\EFI\\ferrite\\ferrite.toml on the boot volume when an app starts.
##
## Everything is optional. A line starting with `##` explains. A line starting with `# ` is a setting
## that is not set: remove the `# ` to set it. The value on it is the default, or an example where
## there is none. A mistake is logged with its line and key, and only its section is ignored.
";

const WIFI_DOC: &str = "\
## Wi-Fi networks, one [[wifi.networks]] table each. The strongest visible one is tried first, a tie
## goes to the one listed first. At most ";

const NOTIFY_DOC: &str = "\
## Push notifications by POST over plain HTTP (no TLS), as ntfy takes them.
";

const RUN_DOC: &str = "\
## When a run ends by itself: the first part met ends it. With none set it goes on until stopped.
";

fn example_network() -> Network {
    Network {
        ssid: "Home".into(),
        passphrase: Some(Secret::new("correct horse battery")),
        security: Security::Auto,
        hidden: false,
    }
}

fn example_notify() -> Notify {
    Notify { url: Secret::new("http://ntfy.sh/your-topic"), format: NotifyFormat::Text, events: None }
}

impl Config {
    /// The template: every key and its meaning, all of it commented out.
    pub fn template() -> String {
        Self::default().render()
    }

    /// The file for this configuration: the template with the values this holds uncommented. Sections
    /// and keys left at their defaults stay commented.
    pub fn render(&self) -> String {
        let mut out = String::from(HEADER);
        out.push('\n');
        let _ = writeln!(out, "{WIFI_DOC}{MAX_NETWORKS}.");
        if self.wifi.networks.is_empty() {
            network(&mut out, &example_network(), true, true);
        }
        for (i, n) in self.wifi.networks.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            network(&mut out, n, false, i == 0);
        }
        out.push('\n');
        out.push_str(NOTIFY_DOC);
        match &self.notify {
            Some(n) => notify(&mut out, n, false),
            None => notify(&mut out, &example_notify(), true),
        }
        out.push('\n');
        out.push_str(RUN_DOC);
        run(&mut out, &self.run);
        out
    }
}

/// A TOML basic string, escaped by the TOML library.
fn quote(text: &str) -> String {
    toml::Value::String(text.into()).to_string()
}

/// `key = value` under its doc lines, commented out when it is not set.
fn line(out: &mut String, set: bool, key: &str, value: &str, doc: &[&str]) {
    for text in doc {
        let _ = writeln!(out, "## {text}");
    }
    let prefix = if set { "" } else { "# " };
    let _ = writeln!(out, "{prefix}{key} = {value}");
}

fn header(out: &mut String, commented: bool, name: &str) {
    let _ = writeln!(out, "{}{name}", if commented { "# " } else { "" });
}

fn network(out: &mut String, n: &Network, commented: bool, docs: bool) {
    let doc = |text: &'static [&'static str]| if docs { text } else { &[] };
    header(out, commented, "[[wifi.networks]]");
    line(out, !commented, "ssid", &quote(&n.ssid), doc(&["1 to 32 bytes."]));
    if let Some(passphrase) = &n.passphrase {
        let text = &["8 to 63 printable ASCII characters. Leave it out for an open network."];
        line(out, !commented, "passphrase", &quote(passphrase.expose()), doc(text));
    }
    let text = &[
        "auto: WPA3-SAE when the access point offers it, else WPA2-PSK. wpa2: WPA2-PSK only. wpa3: WPA3-SAE only.",
        "open: no encryption, and no passphrase.",
    ];
    line(out, !commented && n.security != Security::Auto, "security", &quote(n.security.name()), doc(text));
    let text = &["True when the network does not broadcast its name, so the scan asks for it."];
    line(out, !commented && n.hidden, "hidden", if n.hidden { "true" } else { "false" }, doc(text));
}

fn notify(out: &mut String, n: &Notify, commented: bool) {
    header(out, commented, "[notify]");
    let text =
        &["A plain http:// URL (no TLS), such as an ntfy topic. It works like a password: keep the file private."];
    line(out, !commented, "url", &quote(n.url.expose()), text);
    let text =
        &["text: the message as the body, the title in a Title header.", "json: {\"event\", \"title\", \"message\"}."];
    line(out, !commented && n.format != NotifyFormat::Text, "format", &quote(n.format.name()), text);
    let events: Vec<String> =
        n.events.as_deref().unwrap_or(&NotifyEvent::ALL).iter().map(|e| quote(e.name())).collect();
    let text = &[
        "Which events to hear about: pass (a pass finished), fault (the first memory fault) and end (the run ended).",
        "All three by default.",
    ];
    line(out, !commented && n.events.is_some(), "events", &alloc::format!("[{}]", events.join(", ")), text);
}

fn run(out: &mut String, r: &Run) {
    header(out, r.is_empty(), "[run]");
    let text = &["A whole number, at least 1. No limit by default."];
    line(out, r.passes.is_some(), "passes", &r.passes.unwrap_or(3).to_string(), text);
    let text = &["Wall time since the run started, like 90m, 2h or 1h30m, from 1s to 365d. No limit by default."];
    let time = r.time.as_deref().map_or_else(|| quote("2h"), quote);
    line(out, r.time.is_some(), "time", &time, text);
    let text = &["End the run at the first memory fault."];
    line(out, r.stop_on_fault, "stop_on_fault", if r.stop_on_fault { "true" } else { "false" }, text);
}
