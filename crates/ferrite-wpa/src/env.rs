//! Networks to join, from `.env` lines.
//!
//! One network is `WPA_SSID="..."` and `WPA_PASSPHRASE="..."`. More are numbered: `WPA_SSID_1`,
//! `WPA_PASSPHRASE_1`, `WPA_SSID_2`, and so on, beside the unnumbered pair, which stays the first
//! network. Each network may also set
//!
//! - `WPA_KEY_MGMT[_N]`: `NONE` for an open network (no passphrase may be given), `WPA-PSK` or `SAE`
//!   to allow only that key management; absent, a passphrase network joins with SAE when the access
//!   point offers it and WPA2-PSK otherwise;
//! - `WPA_HIDDEN[_N]`: `1` (or `true`) when the network does not broadcast its name, so the scan asks
//!   for it by name.
//!
//! One outer pair of double quotes around a value is stripped and nothing is unescaped; blank lines and
//! other keys are ignored. A key given twice takes its last line. Errors name the line and the key, and
//! never a value.

use alloc::string::{String, ToString as _};
use alloc::vec::Vec;
use core::fmt::{self, Write as _};

use snafu::{ResultExt, Snafu};

use crate::kdf::{CredentialError, Passphrase, Ssid};

#[derive(Debug, Clone, PartialEq, Eq, Snafu)]
pub enum EnvError {
    #[snafu(display("no WPA_SSID line"))]
    NoNetworks,
    #[snafu(display("{key} is missing, though line {line} configures the same network"))]
    MissingSsid { key: String, line: usize },
    #[snafu(display("line {line}: {key} needs a {passphrase_key} line"))]
    MissingPassphrase { key: String, passphrase_key: String, line: usize },
    #[snafu(display("line {line}: {key}: {source}"))]
    Invalid { key: String, line: usize, source: CredentialError },
    #[snafu(display("line {line}: {key} is not one of NONE, WPA-PSK, SAE"))]
    KeyMgmt { key: String, line: usize },
    #[snafu(display("line {line}: {key} is not 0, 1, true or false"))]
    Hidden { key: String, line: usize },
    #[snafu(display("line {line}: {key} is set, but {key_mgmt_key} says the network is open"))]
    OpenWithPassphrase { key: String, key_mgmt_key: String, line: usize },
    #[snafu(display("line {line}: {key} does not end in a network number that fits"))]
    Number { key: String, line: usize },
}

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

/// The networks of a file, in the order they are tried when several are visible and equally strong:
/// the unnumbered one, then `_1`, `_2` and so on. Never empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credentials {
    networks: Vec<Network>,
}

/// The keys of one network, with the line each came from.
#[derive(Default)]
struct Entry<'a> {
    ssid: Option<(usize, &'a [u8])>,
    passphrase: Option<(usize, &'a [u8])>,
    key_mgmt: Option<(usize, &'a [u8])>,
    hidden: Option<(usize, &'a [u8])>,
}

#[derive(Clone, Copy)]
enum Field {
    Ssid,
    Passphrase,
    KeyMgmt,
    Hidden,
}

const FIELDS: [(&str, Field); 4] = [
    ("WPA_SSID", Field::Ssid),
    ("WPA_PASSPHRASE", Field::Passphrase),
    ("WPA_KEY_MGMT", Field::KeyMgmt),
    ("WPA_HIDDEN", Field::Hidden),
];

/// The field and network number a key names: `WPA_SSID` is the first network, `WPA_SSID_3` the
/// third. `Ok(None)` for a key that is not ours.
fn classify(key: &str) -> Result<Option<(Field, Option<u32>)>, ()> {
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

fn name(field: Field, slot: Option<u32>) -> String {
    let base = FIELDS.iter().find(|(_, f)| core::mem::discriminant(f) == core::mem::discriminant(&field));
    let base = base.map_or("WPA", |(name, _)| name);
    slot.map_or_else(|| (*base).to_string(), |n| alloc::format!("{base}_{n}"))
}

fn unquote(value: &[u8]) -> &[u8] {
    value.strip_prefix(b"\"").and_then(|r| r.strip_suffix(b"\"")).unwrap_or(value)
}

impl Credentials {
    /// The networks in `text`.
    pub fn from_env(text: &[u8]) -> Result<Self, EnvError> {
        let mut slots: alloc::collections::BTreeMap<Option<u32>, Entry<'_>> = alloc::collections::BTreeMap::new();
        for (n, line) in text.split(|&b| b == b'\n').enumerate() {
            let line_no = n + 1;
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let Some(eq) = line.iter().position(|&b| b == b'=') else { continue };
            let Ok(key) = core::str::from_utf8(&line[..eq]) else { continue };
            let Ok(classified) = classify(key) else {
                return Err(EnvError::Number { key: key.to_string(), line: line_no });
            };
            let Some((field, slot)) = classified else { continue };
            let value = Some((line_no, unquote(&line[eq + 1..])));
            let entry = slots.entry(slot).or_default();
            match field {
                Field::Ssid => entry.ssid = value,
                Field::Passphrase => entry.passphrase = value,
                Field::KeyMgmt => entry.key_mgmt = value,
                Field::Hidden => entry.hidden = value,
            }
        }
        let networks = slots.into_iter().map(|(slot, entry)| entry.network(slot)).collect::<Result<Vec<_>, _>>()?;
        if networks.is_empty() {
            return Err(EnvError::NoNetworks);
        }
        Ok(Self { networks })
    }

    /// A list of exactly one network.
    pub fn single(network: Network) -> Self {
        Self { networks: alloc::vec![network] }
    }

    pub fn networks(&self) -> &[Network] {
        &self.networks
    }
}

impl Entry<'_> {
    fn first_line(&self) -> usize {
        [self.ssid, self.passphrase, self.key_mgmt, self.hidden]
            .into_iter()
            .flatten()
            .map(|(line, _)| line)
            .min()
            .unwrap_or(0)
    }

    fn network(&self, slot: Option<u32>) -> Result<Network, EnvError> {
        let Some((ssid_line, ssid)) = self.ssid else {
            return Err(EnvError::MissingSsid { key: name(Field::Ssid, slot), line: self.first_line() });
        };
        let ssid_key = name(Field::Ssid, slot);
        let ssid = Ssid::new(ssid).context(InvalidSnafu { key: ssid_key, line: ssid_line })?;

        let allow = match self.key_mgmt {
            None => Some(Allow::Any),
            Some((_, b"WPA-PSK")) => Some(Allow::PskOnly),
            Some((_, b"SAE")) => Some(Allow::SaeOnly),
            Some((_, b"NONE")) => None,
            Some((line, _)) => return Err(EnvError::KeyMgmt { key: name(Field::KeyMgmt, slot), line }),
        };
        let hidden = match self.hidden {
            None | Some((_, b"0" | b"false")) => false,
            Some((_, b"1" | b"true")) => true,
            Some((line, _)) => return Err(EnvError::Hidden { key: name(Field::Hidden, slot), line }),
        };
        let auth = match (allow, self.passphrase) {
            (None, None) => Auth::Open,
            (None, Some((line, _))) => {
                return Err(EnvError::OpenWithPassphrase {
                    key: name(Field::Passphrase, slot),
                    key_mgmt_key: name(Field::KeyMgmt, slot),
                    line,
                });
            },
            (Some(_), None) => {
                return Err(EnvError::MissingPassphrase {
                    key: name(Field::Ssid, slot),
                    passphrase_key: name(Field::Passphrase, slot),
                    line: ssid_line,
                });
            },
            (Some(allow), Some((line, passphrase))) => Auth::Personal {
                passphrase: Passphrase::new(passphrase)
                    .context(InvalidSnafu { key: name(Field::Passphrase, slot), line })?,
                allow,
            },
        };
        Ok(Network { ssid, hidden, auth })
    }
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};
    use rstest::rstest;

    use super::*;

    fn personal(text: &[u8]) -> Network {
        Credentials::from_env(text).unwrap().networks()[0].clone()
    }

    #[test]
    fn one_outer_quote_pair_is_stripped_and_nothing_else_changes() {
        let text = b"# comment\r\n\nOTHER=1\nWPA_SSID=\"My \"Home\" Net\"\r\nWPA_PASSPHRASE=\"a\\\"b c d e\"\n";
        let network = personal(text);
        check!(network.ssid.as_bytes() == b"My \"Home\" Net");
        check!(
            network.auth == Auth::Personal { passphrase: Passphrase::new(b"a\\\"b c d e").unwrap(), allow: Allow::Any }
        );
        check!(!network.hidden);
        check!(personal(b"WPA_SSID=lab\nWPA_PASSPHRASE=12345678").ssid.as_bytes() == b"lab");
    }

    #[test]
    fn numbered_networks_follow_the_unnumbered_one_in_number_order() {
        let text = b"WPA_SSID_10=ten\nWPA_PASSPHRASE_10=passphrase10\nWPA_SSID_2=two\nWPA_KEY_MGMT_2=NONE\n\
            WPA_SSID=first\nWPA_PASSPHRASE=passphrase0\nWPA_SSID_01=one\nWPA_PASSPHRASE_01=passphrase1\nWPA_HIDDEN_01=1\n";
        let credentials = Credentials::from_env(text).unwrap();
        let ssids: Vec<&[u8]> = credentials.networks().iter().map(|n| n.ssid.as_bytes()).collect();
        check!(ssids == [&b"first"[..], b"one", b"two", b"ten"]);
        check!(credentials.networks()[1].hidden);
        check!(credentials.networks()[2].auth == Auth::Open);
    }

    #[rstest]
    #[case::absent(b"" as &[u8], Allow::Any)]
    #[case::psk(b"WPA_KEY_MGMT=WPA-PSK\n", Allow::PskOnly)]
    #[case::sae(b"WPA_KEY_MGMT=\"SAE\"\n", Allow::SaeOnly)]
    fn the_key_management_line_limits_a_passphrase_network(#[case] line: &[u8], #[case] want: Allow) {
        let text = [b"WPA_SSID=x\nWPA_PASSPHRASE=12345678\n".as_slice(), line].concat();
        assert!(let Auth::Personal { allow, .. } = personal(&text).auth);
        check!(allow == want);
    }

    #[rstest]
    #[case::one(b"1" as &[u8], true)]
    #[case::word(b"true", true)]
    #[case::zero(b"0", false)]
    #[case::word_false(b"false", false)]
    fn the_hidden_flag_reads_booleans(#[case] value: &[u8], #[case] want: bool) {
        let text = [b"WPA_SSID=x\nWPA_KEY_MGMT=NONE\nWPA_HIDDEN=".as_slice(), value, b"\n"].concat();
        check!(personal(&text).hidden == want);
    }

    #[rstest]
    #[case::no_ssid(b"WPA_PASSPHRASE=12345678\n" as &[u8], EnvError::MissingSsid { key: "WPA_SSID".into(), line: 1 })]
    #[case::no_lines(b"OTHER=1\n", EnvError::NoNetworks)]
    #[case::stray_numbered_key(
        b"WPA_SSID=a\nWPA_PASSPHRASE=12345678\nWPA_PASSPHRASE_2=12345678\n",
        EnvError::MissingSsid { key: "WPA_SSID_2".into(), line: 3 }
    )]
    #[case::no_passphrase(
        b"WPA_SSID_3=x\n",
        EnvError::MissingPassphrase { key: "WPA_SSID_3".into(), passphrase_key: "WPA_PASSPHRASE_3".into(), line: 1 }
    )]
    #[case::bad_key_mgmt(b"WPA_SSID=x\nWPA_KEY_MGMT=WEP\n", EnvError::KeyMgmt { key: "WPA_KEY_MGMT".into(), line: 2 })]
    #[case::bad_hidden(b"WPA_SSID=x\nWPA_HIDDEN=maybe\n", EnvError::Hidden { key: "WPA_HIDDEN".into(), line: 2 })]
    #[case::open_with_passphrase(
        b"WPA_SSID=x\nWPA_KEY_MGMT=NONE\nWPA_PASSPHRASE=12345678\n",
        EnvError::OpenWithPassphrase { key: "WPA_PASSPHRASE".into(), key_mgmt_key: "WPA_KEY_MGMT".into(), line: 3 }
    )]
    #[case::huge_number(b"WPA_SSID_99999999999=x\n", EnvError::Number { key: "WPA_SSID_99999999999".into(), line: 1 })]
    fn a_bad_file_is_refused_with_the_key_and_line_named(#[case] text: &[u8], #[case] want: EnvError) {
        check!(Credentials::from_env(text) == Err(want));
    }

    #[test]
    fn an_invalid_value_names_its_key_and_line_but_never_the_value() {
        let error = Credentials::from_env(b"WPA_SSID=x\nWPA_PASSPHRASE=\"short\"").unwrap_err();
        assert!(let EnvError::Invalid { key, line: 2, source: CredentialError::PassphraseLength { len: 5 } } = &error);
        check!(key == "WPA_PASSPHRASE");
        check!(!error.to_string().contains("short"));
    }

    #[test]
    fn a_network_displays_without_its_passphrase() {
        let network = personal(b"WPA_SSID=Home\nWPA_PASSPHRASE=secret-words\nWPA_HIDDEN=1\n");
        check!(network.to_string() == "\"Home\" WPA2/WPA3, passphrase of 12 characters, hidden");
        check!(personal(b"WPA_SSID=Cafe\nWPA_KEY_MGMT=NONE\n").to_string() == "\"Cafe\" open");
    }
}
