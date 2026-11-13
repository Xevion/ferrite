//! Network credentials from `.env` lines: `WPA_SSID="..."` and `WPA_PASSPHRASE="..."`. One outer
//! pair of double quotes is stripped and nothing is unescaped; blank and other lines are ignored.

use snafu::{ResultExt, Snafu};

use crate::kdf::{CredentialError, Passphrase, Ssid};

#[derive(Debug, Clone, PartialEq, Eq, Snafu)]
pub enum EnvError {
    #[snafu(display("no {key} line"))]
    Missing { key: &'static str },
    #[snafu(display("{key}: {source}"))]
    Invalid { key: &'static str, source: CredentialError },
}

/// A network to join.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credentials {
    pub ssid: Ssid,
    pub passphrase: Passphrase,
}

const SSID_KEY: &str = "WPA_SSID";
const PASSPHRASE_KEY: &str = "WPA_PASSPHRASE";

fn value<'a>(text: &'a [u8], key: &str) -> Option<&'a [u8]> {
    text.split(|&b| b == b'\n').rev().find_map(|line| {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let rest = line.strip_prefix(key.as_bytes())?.strip_prefix(b"=")?;
        Some(rest.strip_prefix(b"\"").and_then(|r| r.strip_suffix(b"\"")).unwrap_or(rest))
    })
}

impl Credentials {
    /// The credentials in `text`; a key given twice takes its last line.
    pub fn from_env(text: &[u8]) -> Result<Self, EnvError> {
        let ssid = value(text, SSID_KEY).ok_or(EnvError::Missing { key: SSID_KEY })?;
        let passphrase = value(text, PASSPHRASE_KEY).ok_or(EnvError::Missing { key: PASSPHRASE_KEY })?;
        Ok(Self {
            ssid: Ssid::new(ssid).context(InvalidSnafu { key: SSID_KEY })?,
            passphrase: Passphrase::new(passphrase).context(InvalidSnafu { key: PASSPHRASE_KEY })?,
        })
    }
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};

    use super::*;

    #[test]
    fn one_outer_quote_pair_is_stripped_and_nothing_else_changes() {
        let text = b"# comment\r\n\nOTHER=1\nWPA_SSID=\"My \"Home\" Net\"\r\nWPA_PASSPHRASE=\"a\\\"b c d e\"\n";
        assert!(let Ok(credentials) = Credentials::from_env(text));
        check!(credentials.ssid.as_bytes() == b"My \"Home\" Net");
        check!(credentials.passphrase == Passphrase::new(b"a\\\"b c d e").unwrap());
        let bare = Credentials::from_env(b"WPA_SSID=lab\nWPA_PASSPHRASE=12345678").unwrap();
        check!(bare.ssid.as_bytes() == b"lab");
    }

    #[test]
    fn a_missing_or_invalid_key_is_named() {
        check!(Credentials::from_env(b"WPA_SSID=\"x\"\n") == Err(EnvError::Missing { key: "WPA_PASSPHRASE" }));
        assert!(let Err(EnvError::Invalid { key: "WPA_PASSPHRASE", .. }) =
            Credentials::from_env(b"WPA_SSID=x\nWPA_PASSPHRASE=\"short\""));
    }
}
