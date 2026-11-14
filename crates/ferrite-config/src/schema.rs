//! The data of the file as the file spells it. Only its shape is checked here (types, unknown keys);
//! the values are checked by [`crate::load`], through the validated types the consumers use.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use serde::Deserialize;

/// Everything the file can say; each part is optional.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    pub wifi: Wifi,
    pub notify: Option<Notify>,
    pub run: Run,
}

/// A value that must not reach a log: a passphrase, or a URL that works like one. Its `Debug` hides it.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The value itself, for the one place that needs it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

/// `[[wifi.networks]]`: the networks to join, in file order.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Wifi {
    pub networks: Vec<Network>,
}

/// One `[[wifi.networks]]` table.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Network {
    /// 1 to 32 bytes.
    pub ssid: String,
    /// 8 to 63 printable ASCII characters; none for an open network.
    #[serde(default)]
    pub passphrase: Option<Secret>,
    #[serde(default)]
    pub security: Security,
    /// The network does not broadcast its name, so the scan asks for it by name.
    #[serde(default)]
    pub hidden: bool,
}

/// How a network is secured.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Security {
    /// WPA3-SAE when the access point offers it, WPA2-PSK otherwise; a passphrase is needed.
    #[default]
    Auto,
    /// No encryption; no passphrase.
    Open,
    /// WPA2-PSK only.
    Wpa2,
    /// WPA3-SAE only.
    Wpa3,
}

impl Security {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Open => "open",
            Self::Wpa2 => "wpa2",
            Self::Wpa3 => "wpa3",
        }
    }
}

/// `[notify]`: where to POST push notifications, in what body format, for which events.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notify {
    /// A plain `http://` URL, such as an ntfy topic.
    pub url: Secret,
    #[serde(default)]
    pub format: NotifyFormat,
    /// The events to hear about; all of them when absent.
    #[serde(default)]
    pub events: Option<Vec<NotifyEvent>>,
}

/// How the notification's body is written.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotifyFormat {
    /// The message as plain text, the title in a `Title` header: what ntfy takes.
    #[default]
    Text,
    /// `{"event":...,"title":...,"message":...}`.
    Json,
}

impl NotifyFormat {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Json => "json",
        }
    }
}

/// Something worth telling the user about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotifyEvent {
    /// A pass over every test finished.
    Pass,
    /// The first fault of the run was found.
    Fault,
    /// The run ended.
    End,
}

impl NotifyEvent {
    pub const ALL: [Self; 3] = [Self::Pass, Self::Fault, Self::End];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fault => "fault",
            Self::End => "end",
        }
    }
}

/// `[run]`: when a run is done; the first part met ends it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Run {
    /// Passes over the memory, at least 1.
    pub passes: Option<u64>,
    /// Wall time since the run started, like `90m`, `2h` or `1h30m`.
    pub time: Option<String>,
    /// End the run at the first memory fault.
    pub stop_on_fault: bool,
}

impl Run {
    /// Whether the section says nothing.
    pub const fn is_empty(&self) -> bool {
        self.passes.is_none() && self.time.is_none() && !self.stop_on_fault
    }
}
