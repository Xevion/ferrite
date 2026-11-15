//! Which machine this is, as a number the timings file can be keyed by.
//!
//! The firmware's SMBIOS system UUID when it has a real one, found through the configuration table's
//! entry point; otherwise the CPU's brand string and signature from CPUID. Either way the number is
//! a hash, so the file never holds the UUID itself. How much memory is under test is not part of it:
//! that is a field of its own in the timings key.

use core::fmt;

use ferrite_efi::sensors;

use crate::smbios::{self, Structures};
use crate::timings::fnv1a;

/// What the machine's number was made from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    SmbiosUuid,
    CpuBrand,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::SmbiosUuid => "the SMBIOS system UUID",
            Self::CpuBrand => "the CPU brand string",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Machine {
    pub id: u64,
    pub source: Source,
}

impl Machine {
    /// For tests and hosts: a machine with no firmware to ask.
    pub const UNKNOWN: Self = Self { id: 0, source: Source::CpuBrand };

    /// This machine, from the firmware if it has an identity worth the name.
    #[must_use]
    pub fn detect() -> Self {
        let uuid = smbios::with_table(system_uuid).flatten();
        let machine = Self::identify(uuid, &sensors::fingerprint());
        log::info!("machine: {:016x}, from {}", machine.id, machine.source);
        machine
    }

    /// The firmware's UUID when it gave one, else the CPU's fingerprint.
    fn identify(uuid: Option<[u8; 16]>, cpu: &[u8; 52]) -> Self {
        uuid.map_or_else(
            || Self { id: fnv1a(cpu), source: Source::CpuBrand },
            |uuid| Self { id: fnv1a(&uuid), source: Source::SmbiosUuid },
        )
    }
}

/// The UUID of the System Information structure (type 1) in an SMBIOS structure table, unless it is
/// all zeros or all ones, which is how firmware says it has none.
fn system_uuid(table: &[u8]) -> Option<[u8; 16]> {
    let system = Structures::new(table).find(|s| s.kind == 1)?;
    let uuid: [u8; 16] = system.bytes_at(8, 16)?.try_into().ok()?;
    let unset = uuid.iter().all(|&b| b == 0) || uuid.iter().all(|&b| b == 0xff);
    (!unset).then_some(uuid)
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use assert2::check;

    use super::*;

    /// An SMBIOS structure: header, formatted area filled with `fill`, then its strings.
    fn structure(kind: u8, len: u8, fill: &[u8], strings: &[u8]) -> Vec<u8> {
        let mut s = alloc::vec![kind, len, 0x01, 0x00];
        s.extend_from_slice(fill);
        s.resize(usize::from(len), 0);
        s.extend_from_slice(strings);
        s
    }

    const UUID: [u8; 16] =
        [0x4c, 0x4c, 0x44, 0x44, 0x00, 0x10, 0x58, 0x80, 0xc0, 0xc0, 0xc0, 0x4f, 0x4e, 0x53, 0x32, 0x33];

    /// A BIOS structure, the system structure with `uuid`, and the end marker.
    fn table(uuid: &[u8; 16]) -> Vec<u8> {
        let mut t = structure(0, 0x1a, &[1, 2], b"vendor\0version\0\0");
        let mut system = alloc::vec![1, 0x1b, 2, 0, 1, 2, 3, 4];
        system.extend_from_slice(uuid);
        system.extend_from_slice(&[6, 5, 6]);
        system.extend_from_slice(b"maker\0\0");
        t.extend(system);
        t.extend(structure(127, 4, &[], b"\0\0"));
        t
    }

    #[test]
    fn the_uuid_is_the_system_information_structure_past_the_ones_before_it() {
        check!(system_uuid(&table(&UUID)) == Some(UUID));
    }

    #[test]
    fn a_uuid_the_firmware_left_unset_is_no_identity() {
        check!(system_uuid(&table(&[0; 16])).is_none());
        check!(system_uuid(&table(&[0xff; 16])).is_none());
    }

    #[test]
    fn a_damaged_table_gives_nothing_and_does_not_panic() {
        let good = table(&UUID);
        for cut in 0..good.len() {
            let _ = system_uuid(&good[..cut]);
        }
        check!(system_uuid(&good[..10]).is_none());
        check!(system_uuid(&[]).is_none());
        // A structure that claims no length cannot be stepped over.
        check!(system_uuid(&[0, 0, 0, 0, 0, 0]).is_none());
    }

    #[test]
    fn the_cpu_identity_hashes_the_raw_brand_bytes_and_signature() {
        // The input of the id that saved timings are keyed by; it must not change.
        let brand = b"Intel(R) Core(TM) i7-8650U CPU @ 1.90GHz";
        let mut cpu = [0_u8; 52];
        cpu[..brand.len()].copy_from_slice(brand);
        cpu[48..].copy_from_slice(&0x0008_06ea_u32.to_le_bytes());
        let machine = Machine::identify(None, &cpu);
        check!(machine == Machine { id: 0xa131_497c_20d7_c3af, source: Source::CpuBrand });
    }

    #[test]
    fn a_uuid_wins_over_the_cpu() {
        let machine = Machine::identify(Some(UUID), &[0; 52]);
        check!(machine == Machine { id: fnv1a(&UUID), source: Source::SmbiosUuid });
    }
}
