//! Random bits for identifiers: the firmware RNG, or the TSC when there is none.

use alloc::vec::Vec;
use core::mem::ManuallyDrop;

use snafu::Snafu;
use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams, ScopedProtocol};
use uefi::proto::rng::{Rng, RngAlgorithmType};

use crate::clock;

/// A random `u64`. Not for anything secret: the fallback is the raw TSC.
#[must_use]
pub fn random_u64() -> u64 {
    firmware_random().unwrap_or_else(|| {
        log::debug!("no RNG protocol, using the TSC for random bits");
        clock::tsc_entropy()
    })
}

fn firmware_random() -> Option<u64> {
    let mut bytes = [0; 8];
    firmware_bytes(&mut bytes).ok()?;
    Some(u64::from_ne_bytes(bytes))
}

/// The RNG protocol, opened shared. The open is never closed, because some firmware does not track a
/// `GetProtocol` open and fails the close.
fn open_rng() -> Option<ManuallyDrop<ScopedProtocol<Rng>>> {
    let handle = boot::get_handle_for_protocol::<Rng>().ok()?;
    let params = OpenProtocolParams { handle, agent: boot::image_handle(), controller: None };
    // SAFETY: a shared open of a protocol whose only calls here are `get_info` and `get_rng`.
    unsafe { boot::open_protocol::<Rng>(params, OpenProtocolAttributes::GetProtocol) }.ok().map(ManuallyDrop::new)
}

#[derive(Debug, Snafu)]
pub enum FirmwareRngError {
    #[snafu(display("the firmware has no RNG protocol"))]
    NoProtocol,
    #[snafu(display("the firmware RNG failed to produce {len} bytes"))]
    Failed { len: usize, source: uefi::Error },
}

/// Fill `buf` from the firmware RNG's default algorithm; never falls back to a weaker source.
pub fn firmware_bytes(buf: &mut [u8]) -> Result<(), FirmwareRngError> {
    let mut rng = open_rng().ok_or(FirmwareRngError::NoProtocol)?;
    rng.get_rng(None, buf).map_err(|source| FirmwareRngError::Failed { len: buf.len(), source })
}

/// The algorithms the firmware RNG lists, or `None` without the protocol.
pub fn firmware_algorithms() -> Option<Vec<RngAlgorithmType>> {
    let mut rng = open_rng()?;
    let mut list = [RngAlgorithmType::EMPTY_ALGORITHM; 8];
    rng.get_info(&mut list).ok().map(<[_]>::to_vec)
}
