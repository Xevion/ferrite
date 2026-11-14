//! The networks to join: what each one needs and how its name reads in a log.
//!
//! Parsing a file into these is the settings file's job (`ferrite-config`), which checks each value
//! through [`Ssid::new`] and [`Passphrase::new`].

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::{self, Write as _};

use crate::kdf::{Passphrase, Ssid};

/// Which key management a passphrase network may be joined with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Allow {
    /// WPA3-SAE when the access point offers it, WPA2-PSK otherwise.
    Any,
    PskOnly,
    SaeOnly,
}

/// How a network is secured, and what is needed to join it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Auth {
    /// No encryption and no handshake.
    Open,
    /// WPA2-PSK or WPA3-SAE with this passphrase (the SAE password).
    Personal { passphrase: Passphrase, allow: Allow },
}

/// A network to join.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Network {
    pub ssid: Ssid,
    /// The network does not broadcast its name: scans ask for it by name.
    pub hidden: bool,
    pub auth: Auth,
}

impl fmt::Display for Network {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "\"{}\" ", escape_ssid(self.ssid.as_bytes()))?;
        match &self.auth {
            Auth::Open => f.write_str("open")?,
            Auth::Personal { passphrase, allow } => {
                let allow = match allow {
                    Allow::Any => "WPA2/WPA3",
                    Allow::PskOnly => "WPA2-PSK",
                    Allow::SaeOnly => "WPA3-SAE",
                };
                write!(f, "{allow}, passphrase of {} characters", passphrase.len())?;
            },
        }
        if self.hidden { f.write_str(", hidden") } else { Ok(()) }
    }
}

/// An SSID for a log line: UTF-8 as is, other bytes escaped.
fn escape_ssid(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            if c.is_control() {
                let _ = write!(out, "\\x{:02x}", u32::from(c));
            } else {
                out.push(c);
            }
        }
        for b in chunk.invalid() {
            let _ = write!(out, "\\x{b:02x}");
        }
    }
    out
}

/// The networks to try when several are visible and equally strong, in order. Never empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credentials {
    networks: Vec<Network>,
}

impl Credentials {
    /// The networks in `networks`; `None` when it is empty.
    pub fn new(networks: Vec<Network>) -> Option<Self> {
        (!networks.is_empty()).then_some(Self { networks })
    }

    /// A list of exactly one network.
    pub fn single(network: Network) -> Self {
        Self { networks: alloc::vec![network] }
    }

    pub fn networks(&self) -> &[Network] {
        &self.networks
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString as _;

    use assert2::{assert, check};

    use super::*;

    fn network(ssid: &[u8], passphrase: Option<&[u8]>, hidden: bool) -> Network {
        let auth = passphrase
            .map_or(Auth::Open, |p| Auth::Personal { passphrase: Passphrase::new(p).unwrap(), allow: Allow::Any });
        Network { ssid: Ssid::new(ssid).unwrap(), hidden, auth }
    }

    #[test]
    fn a_network_displays_without_its_passphrase() {
        let home = network(b"Home", Some(b"secret-words"), true);
        check!(home.to_string() == "\"Home\" WPA2/WPA3, passphrase of 12 characters, hidden");
        check!(network(b"Cafe", None, false).to_string() == "\"Cafe\" open");
        check!(!alloc::format!("{home:?}").contains("secret-words"));
    }

    #[test]
    fn credentials_are_never_empty() {
        assert!(Credentials::new(Vec::new()).is_none());
        assert!(let Some(credentials) = Credentials::new(alloc::vec![network(b"a", None, false)]));
        check!(credentials.networks().len() == 1);
    }
}
