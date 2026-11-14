//! WPA2-PSK and WPA3-SAE for a station, in the way iwd and `wpa_supplicant` run them: the PMK from the
//! passphrase or from the SAE exchange ([`sae`]), the PTK from the 4-way handshake's nonces, EAPOL-Key
//! frames with their MIC and wrapped key data, the 4-way and group key handshakes as state machines that
//! hand back replies and keys, and the networks a `.env` file names ([`env`]).
//!
//! Platform-free: frames come in and go out as bytes, keys are returned for the caller to install,
//! and nonces come from an injected [`Rng`]. The `authenticator` feature adds the access point side.

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

#[cfg(feature = "authenticator")]
pub mod authenticator;
pub mod eapol;
pub mod env;
pub mod kdf;
pub mod rsn;
pub mod sae;
pub mod supplicant;

/// A 48-bit IEEE MAC address.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct MacAddr(pub [u8; 6]);

/// Where nonces come from; must be a cryptographic source.
pub trait Rng {
    fn fill(&mut self, buf: &mut [u8]);
}
