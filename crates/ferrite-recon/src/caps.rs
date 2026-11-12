//! The xHCI extended capability list.

use alloc::vec::Vec;

use ferrite_hal::{Mmio, MmioExt, PhysAddr};
use ferrite_xhci::regs::{ExtCapHeader, xcap};

/// A malformed list could loop forever; real controllers have a handful of entries.
pub const WALK_LIMIT: usize = 64;

#[derive(Clone, Copy, Debug)]
pub struct ExtCap {
    pub at: PhysAddr,
    pub header: ExtCapHeader,
}

/// Walk the list whose first entry is `xecp` dwords above `base`, stopping at the end
/// marker, an unreadable or all-ones header, or [`WALK_LIMIT`] entries.
pub fn walk<M: Mmio>(mmio: &mut M, base: PhysAddr, xecp: u16) -> Vec<ExtCap> {
    let mut caps = Vec::new();
    if xecp == 0 {
        return caps;
    }
    let mut at = base.offset(u64::from(xecp) * 4);
    for _ in 0..WALK_LIMIT {
        let Ok(header) = mmio.try_read(at, xcap::HEADER) else { break };
        if header.raw_value() == u32::MAX {
            break;
        }
        caps.push(ExtCap { at, header });
        if header.next() == 0 {
            break;
        }
        at = at.offset(u64::from(header.next()) * 4);
    }
    caps
}

#[cfg(test)]
mod tests {
    use alloc::collections::BTreeMap;

    use assert2::check;
    use ferrite_hal::PlatformError;

    use super::*;

    struct Memory(BTreeMap<u64, u32>);

    impl Mmio for Memory {
        fn try_read32(&mut self, addr: PhysAddr) -> Result<u32, PlatformError> {
            self.0.get(&addr.get()).copied().ok_or(PlatformError::new(1))
        }

        fn try_write32(&mut self, _: PhysAddr, _: u32) -> Result<(), PlatformError> {
            Ok(())
        }
    }

    struct Endless;

    impl Mmio for Endless {
        fn try_read32(&mut self, _: PhysAddr) -> Result<u32, PlatformError> {
            Ok(0x0000_0101)
        }

        fn try_write32(&mut self, _: PhysAddr, _: u32) -> Result<(), PlatformError> {
            Ok(())
        }
    }

    #[test]
    fn walk_follows_next_pointers_and_stops_at_all_ones() {
        let mut mem = Memory(BTreeMap::from([
            (0x1000, 0x0300_0401),
            (0x1010, 0x0000_0402),
            (0x1020, u32::MAX),
            (0x1030, 0x0000_000a),
        ]));
        let caps = walk(&mut mem, PhysAddr::new(0), 0x400);
        let ids: Vec<_> = caps.iter().map(|c| (c.at.get(), c.header.id())).collect();
        check!(ids == [(0x1000, 0x01), (0x1010, 0x02)]);
    }

    #[test]
    fn walk_is_capped_on_an_endless_list() {
        check!(walk(&mut Endless, PhysAddr::new(0), 0x400).len() == WALK_LIMIT);
    }

    #[test]
    fn no_pointer_means_no_capabilities() {
        check!(walk(&mut Endless, PhysAddr::new(0), 0).is_empty());
    }
}
