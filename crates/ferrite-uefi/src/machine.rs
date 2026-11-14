//! Which machine this is, as a number the timings file can be keyed by.
//!
//! The firmware's SMBIOS system UUID when it has a real one, found through the configuration table's
//! entry point; otherwise the CPU's brand string and signature from CPUID. Either way the number is
//! a hash, so the file never holds the UUID itself. How much memory is under test is not part of it:
//! that is a field of its own in the timings key.

use core::arch::x86_64::__cpuid;
use core::ffi::c_void;
use core::fmt;

use uefi::system;
use uefi::table::cfg::ConfigTableEntry;

use crate::smbios::Structures;
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
        let machine = smbios_table()
            .and_then(|table| system_uuid(&table))
            .map_or_else(cpu_brand, |uuid| Self { id: fnv1a(&uuid), source: Source::SmbiosUuid });
        log::info!("machine: {:016x}, from {}", machine.id, machine.source);
        machine
    }
}

/// An SMBIOS entry point's table: its address and length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TableAt {
    address: u64,
    len: usize,
}

/// The structure table an entry point describes: the 64-bit `_SM3_` entry point or the 32-bit `_SM_` one.
fn table_at(entry: &[u8]) -> Option<TableAt> {
    let le32 = |at: usize| entry.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    if entry.starts_with(b"_SM3_") {
        let address = entry.get(0x10..0x18).map(|b| u64::from_le_bytes(b.try_into().unwrap_or([0; 8])))?;
        return Some(TableAt { address, len: le32(0x0c)? as usize });
    }
    if entry.starts_with(b"_SM_") {
        let len = entry.get(0x16..0x18).map(|b| u16::from_le_bytes([b[0], b[1]]))?;
        return Some(TableAt { address: u64::from(le32(0x18)?), len: usize::from(len) });
    }
    None
}

/// The UUID of the System Information structure (type 1) in an SMBIOS structure table, unless it is
/// all zeros or all ones, which is how firmware says it has none.
fn system_uuid(table: &[u8]) -> Option<[u8; 16]> {
    let system = Structures::new(table).find(|s| s.kind == 1)?;
    let uuid: [u8; 16] = system.bytes_at(8, 16)?.try_into().ok()?;
    let unset = uuid.iter().all(|&b| b == 0) || uuid.iter().all(|&b| b == 0xff);
    (!unset).then_some(uuid)
}

/// The SMBIOS structure table the firmware published, copied out of its memory.
pub(crate) fn smbios_table() -> Option<alloc::vec::Vec<u8>> {
    /// Larger than any real structure table, so a corrupt length cannot make this read far.
    const MAX_TABLE: usize = 256 << 10;
    let entry_of =
        |guid| system::with_config_table(|entries| entries.iter().find(|e| e.guid == guid).map(|e| e.address));
    let entry: *const c_void =
        entry_of(ConfigTableEntry::SMBIOS3_GUID).or_else(|| entry_of(ConfigTableEntry::SMBIOS_GUID))?;
    // SAFETY: the configuration table names the entry point, which is at least 0x1f bytes in either format.
    let header = unsafe { core::slice::from_raw_parts(entry.cast::<u8>(), 0x20) };
    let TableAt { address, len } = table_at(header)?;
    if len == 0 || len > MAX_TABLE {
        return None;
    }
    // SAFETY: the entry point gives this address and length for the structure table, which the
    // firmware keeps in memory it does not hand out to anyone else.
    let table = unsafe { core::slice::from_raw_parts(usize::try_from(address).ok()? as *const u8, len) };
    Some(table.to_vec())
}

/// The CPU's brand string and signature, for a machine whose firmware has no UUID.
fn cpu_brand() -> Machine {
    let mut text = [0_u8; 48 + 4];
    let max = __cpuid(0x8000_0000).eax;
    if max >= 0x8000_0004 {
        for (i, leaf) in (0x8000_0002..=0x8000_0004_u32).enumerate() {
            let r = __cpuid(leaf);
            for (j, reg) in [r.eax, r.ebx, r.ecx, r.edx].into_iter().enumerate() {
                text[i * 16 + j * 4..][..4].copy_from_slice(&reg.to_le_bytes());
            }
        }
    }
    text[48..].copy_from_slice(&__cpuid(1).eax.to_le_bytes());
    Machine { id: fnv1a(&text), source: Source::CpuBrand }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use assert2::{assert, check};

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
    fn entry_points_give_the_table() {
        let mut v3 = alloc::vec![0_u8; 0x18];
        v3[..5].copy_from_slice(b"_SM3_");
        v3[0x0c..0x10].copy_from_slice(&0x1234_u32.to_le_bytes());
        v3[0x10..0x18].copy_from_slice(&0x7abc_d000_u64.to_le_bytes());
        check!(table_at(&v3) == Some(TableAt { address: 0x7abc_d000, len: 0x1234 }));
        let mut v2 = alloc::vec![0_u8; 0x1f];
        v2[..4].copy_from_slice(b"_SM_");
        v2[0x16..0x18].copy_from_slice(&0x0abc_u16.to_le_bytes());
        v2[0x18..0x1c].copy_from_slice(&0x000f_0000_u32.to_le_bytes());
        check!(table_at(&v2) == Some(TableAt { address: 0xf_0000, len: 0xabc }));
        assert!(table_at(b"_DMI_").is_none());
        check!(table_at(&v3[..0x10]).is_none());
    }
}
