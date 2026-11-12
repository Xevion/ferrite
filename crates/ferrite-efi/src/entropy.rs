//! Random bits for identifiers: the firmware RNG, or the TSC when there is none.

use core::mem::ManuallyDrop;

use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams};
use uefi::proto::rng::Rng;

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
    let handle = boot::get_handle_for_protocol::<Rng>().ok()?;
    let params = OpenProtocolParams { handle, agent: boot::image_handle(), controller: None };
    // SAFETY: a shared open of a protocol whose only call here is one `get_rng`. The open is never
    // closed, because some firmware does not track a `GetProtocol` open and fails the close.
    let mut rng =
        ManuallyDrop::new(unsafe { boot::open_protocol::<Rng>(params, OpenProtocolAttributes::GetProtocol) }.ok()?);
    let mut bytes = [0; 8];
    rng.get_rng(None, &mut bytes).ok()?;
    Some(u64::from_ne_bytes(bytes))
}
