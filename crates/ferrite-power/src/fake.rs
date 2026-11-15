//! A processor made of tables, for tests.

use std::collections::BTreeMap;
use std::vec::Vec;

use crate::cpuid::{CpuidSource, Leaf};
use crate::msr::{Fault, MsrIo};

/// MSRs as a map: an absent address faults, and every write is recorded.
#[derive(Default)]
pub struct Msrs {
    pub values: BTreeMap<u32, u64>,
    pub writes: Vec<(u32, u64)>,
}

impl Msrs {
    pub fn with(registers: &[(u32, u64)]) -> Self {
        Self { values: registers.iter().copied().collect(), writes: Vec::new() }
    }
}

impl MsrIo for Msrs {
    fn read_raw(&mut self, address: u32) -> Result<u64, Fault> {
        self.values.get(&address).copied().ok_or(Fault { address })
    }

    fn write_raw(&mut self, address: u32, value: u64) -> Result<(), Fault> {
        let slot = self.values.get_mut(&address).ok_or(Fault { address })?;
        *slot = value;
        self.writes.push((address, value));
        Ok(())
    }
}

/// CPUID as a map from leaf (subleaf 0) to registers; an absent leaf reads as zeros.
pub struct Cpuid(pub BTreeMap<u32, Leaf>);

impl Cpuid {
    pub fn with(leaves: &[(u32, [u32; 4])]) -> Self {
        Self(leaves.iter().map(|&(n, [eax, ebx, ecx, edx])| (n, Leaf { eax, ebx, ecx, edx })).collect())
    }
}

impl CpuidSource for Cpuid {
    fn cpuid(&self, leaf: u32, _subleaf: u32) -> Leaf {
        self.0.get(&leaf).copied().unwrap_or_default()
    }
}

/// The vendor string's EBX, ECX and EDX.
pub fn vendor(text: &[u8; 12]) -> [u32; 3] {
    let word = |i: usize| u32::from_le_bytes([text[i], text[i + 1], text[i + 2], text[i + 3]]);
    [word(0), word(8), word(4)]
}
