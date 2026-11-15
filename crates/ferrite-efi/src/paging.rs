//! Memory types in the firmware's identity map: the PAT, the MTRRs over an address, and remapping a
//! range (a GOP framebuffer) write-combining for an app's lifetime.
//!
//! Firmware usually leaves the framebuffer aperture uncacheable, where every store is its own bus
//! transaction. Write-combining batches stores into 64-byte bursts. A PAT entry of type WC wins over
//! an uncacheable MTRR, so the remap needs only the PAT and the leaf entries over the range: no MTRR
//! is touched. Large pages straddling an end of the range are split so nothing outside it changes.

use alloc::vec::Vec;
use core::fmt;
use core::ptr::NonNull;

use ferrite_hal::PhysAddr;
use snafu::{Snafu, ensure};
use uefi::boot::{self, AllocateType, MemoryType};

use crate::x86::{read_cr0, read_cr3, read_cr4, read_msr, without_interrupts, write_cr0, write_cr4, write_msr};

const IA32_PAT: u32 = 0x277;
const IA32_MTRRCAP: u32 = 0xfe;
const IA32_MTRR_DEF_TYPE: u32 = 0x2ff;
const IA32_MTRR_PHYSBASE0: u32 = 0x200;

const PRESENT: u64 = 1;
const WRITABLE: u64 = 1 << 1;
const PWT: u64 = 1 << 3;
const PCD: u64 = 1 << 4;
/// Page size in a PDPT or PD entry; the PAT bit in a PT entry.
const PS: u64 = 1 << 7;
const PAT_LARGE: u64 = 1 << 12;
const ADDR: u64 = 0x000f_ffff_ffff_f000;
const ADDR_LARGE: u64 = 0x000f_ffff_ffff_e000;

const CR0_WP: u64 = 1 << 16;
const CR4_PGE: u64 = 1 << 7;
const CR4_LA57: u64 = 1 << 12;

const PAGE: u64 = 4096;
const ENTRIES: usize = 512;
/// Splits one range can need: at each end, a 1 GiB page into 2 MiB pages and a 2 MiB page into 4 KiB.
const MAX_SPLITS: usize = 4;

/// A memory type as the PAT and the MTRRs encode it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemType {
    Uncacheable,
    WriteCombining,
    WriteThrough,
    WriteProtected,
    WriteBack,
    /// PAT only: uncacheable unless an MTRR says write-combining.
    UncachedMinus,
    Unknown(u8),
}

impl MemType {
    const fn from_raw(raw: u8) -> Self {
        match raw {
            0 => Self::Uncacheable,
            1 => Self::WriteCombining,
            4 => Self::WriteThrough,
            5 => Self::WriteProtected,
            6 => Self::WriteBack,
            7 => Self::UncachedMinus,
            other => Self::Unknown(other),
        }
    }

    const fn raw(self) -> u8 {
        match self {
            Self::Uncacheable => 0,
            Self::WriteCombining => 1,
            Self::WriteThrough => 4,
            Self::WriteProtected => 5,
            Self::WriteBack => 6,
            Self::UncachedMinus => 7,
            Self::Unknown(raw) => raw,
        }
    }
}

impl fmt::Display for MemType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Uncacheable => f.write_str("UC"),
            Self::WriteCombining => f.write_str("WC"),
            Self::WriteThrough => f.write_str("WT"),
            Self::WriteProtected => f.write_str("WP"),
            Self::WriteBack => f.write_str("WB"),
            Self::UncachedMinus => f.write_str("UC-"),
            Self::Unknown(raw) => write!(f, "type {raw}"),
        }
    }
}

/// The eight PAT entries, indexed by a leaf's PAT, PCD and PWT bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pat(u64);

impl Pat {
    #[must_use]
    pub fn read() -> Self {
        // SAFETY: the PAT MSR exists on every x86_64 CPU.
        Self(unsafe { read_msr(IA32_PAT) })
    }

    #[must_use]
    pub const fn entry(self, index: u8) -> MemType {
        MemType::from_raw((self.0 >> (index * 8)) as u8 & 7)
    }

    const fn with(self, index: u8, kind: MemType) -> Self {
        let shift = index * 8;
        Self(self.0 & !(0xff << shift) | (kind.raw() as u64) << shift)
    }

    fn find(self, kind: MemType) -> Option<u8> {
        (0..8).find(|&i| self.entry(i) == kind)
    }
}

impl fmt::Display for Pat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for i in 0..8 {
            write!(f, "{}{}", if i == 0 { "" } else { " " }, self.entry(i))?;
        }
        Ok(())
    }
}

/// The MTRRs' say over one address: the default type and the variable ranges covering it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MtrrView {
    pub enabled: bool,
    pub default: MemType,
    /// The type of the first enabled variable range covering the address.
    pub covering: Option<MemType>,
}

impl MtrrView {
    /// The MTRRs as they apply to `addr` (variable ranges only; fixed ranges cover the first MiB).
    #[must_use]
    pub fn at(addr: PhysAddr) -> Self {
        // SAFETY: the MTRR MSRs exist on every x86_64 CPU.
        let (cap, def) = unsafe { (read_msr(IA32_MTRRCAP), read_msr(IA32_MTRR_DEF_TYPE)) };
        let ranges = (cap & 0xff) as u32;
        let covering = (0..ranges).find_map(|i| {
            // SAFETY: `i` is below the count MTRRCAP reports.
            let (base, mask) =
                unsafe { (read_msr(IA32_MTRR_PHYSBASE0 + 2 * i), read_msr(IA32_MTRR_PHYSBASE0 + 2 * i + 1)) };
            let valid = mask & (1 << 11) != 0;
            let mask = mask & ADDR;
            (valid && addr.get() & mask == base & ADDR & mask).then(|| MemType::from_raw(base as u8))
        });
        Self { enabled: def & (1 << 11) != 0, default: MemType::from_raw(def as u8), covering }
    }
}

impl fmt::Display for MtrrView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.enabled {
            return f.write_str("MTRRs off");
        }
        match self.covering {
            Some(kind) => write!(f, "MTRR {kind}"),
            None => write!(f, "MTRR default {}", self.default),
        }
    }
}

/// Levels of the 4-level map, from the top.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Level {
    Pml4,
    Pdpt,
    Pd,
    Pt,
}

impl Level {
    const fn shift(self) -> u32 {
        match self {
            Self::Pml4 => 39,
            Self::Pdpt => 30,
            Self::Pd => 21,
            Self::Pt => 12,
        }
    }

    const fn size(self) -> u64 {
        1 << self.shift()
    }

    const fn below(self) -> Option<Self> {
        match self {
            Self::Pml4 => Some(Self::Pdpt),
            Self::Pdpt => Some(Self::Pd),
            Self::Pd => Some(Self::Pt),
            Self::Pt => None,
        }
    }

    /// Whether an entry at this level maps a page rather than pointing at a table.
    const fn is_leaf(self, entry: u64) -> bool {
        match self {
            Self::Pml4 => false,
            Self::Pdpt | Self::Pd => entry & PS != 0,
            Self::Pt => true,
        }
    }

    const fn pat_bit(self) -> u64 {
        if matches!(self, Self::Pt) { PS } else { PAT_LARGE }
    }

    const fn index(self, addr: u64) -> usize {
        (addr >> self.shift()) as usize % ENTRIES
    }

    /// A leaf's PAT index: its PAT, PCD and PWT bits.
    const fn pat_index(self, entry: u64) -> u8 {
        (entry & PWT != 0) as u8 | ((entry & PCD != 0) as u8) << 1 | ((entry & self.pat_bit() != 0) as u8) << 2
    }

    const fn with_pat_index(self, entry: u64, index: u8) -> u64 {
        let mut e = entry & !(PWT | PCD | self.pat_bit());
        if index & 1 != 0 {
            e |= PWT;
        }
        if index & 2 != 0 {
            e |= PCD;
        }
        if index & 4 != 0 {
            e |= self.pat_bit();
        }
        e
    }

    const fn frame(self, entry: u64) -> u64 {
        if matches!(self, Self::Pdpt | Self::Pd) && entry & PS != 0 { entry & ADDR_LARGE } else { entry & ADDR }
    }
}

/// A leaf mapping as found by a walk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Leaf {
    /// Page size in bytes.
    pub size: u64,
    pub pat_index: u8,
    pub writable: bool,
}

/// Where page tables live: the identity map on the target, a fake in host tests.
trait Tables {
    fn root(&self) -> u64;
    /// The table at physical address `phys`.
    fn table(&mut self, phys: u64) -> *mut [u64; ENTRIES];
}

/// The CPU's own tables, through the firmware's identity map.
struct Live;

impl Tables for Live {
    fn root(&self) -> u64 {
        read_cr3() & ADDR
    }

    fn table(&mut self, phys: u64) -> *mut [u64; ENTRIES] {
        core::ptr::with_exposed_provenance_mut(phys as usize)
    }
}

/// The leaf mapping `addr`, if any.
fn leaf_in(tables: &mut impl Tables, addr: u64) -> Option<Leaf> {
    let mut table = tables.root();
    let mut level = Level::Pml4;
    loop {
        // SAFETY: `table` came from a present entry of the walk (or CR3), so it is a page table.
        let entry = unsafe { (*tables.table(table))[level.index(addr)] };
        if entry & PRESENT == 0 {
            return None;
        }
        if level.is_leaf(entry) {
            let writable = entry & WRITABLE != 0;
            return Some(Leaf { size: level.size(), pat_index: level.pat_index(entry), writable });
        }
        table = entry & ADDR;
        level = level.below()?;
    }
}

/// How `addr` is mapped now, through the CPU's own tables.
#[must_use]
pub fn leaf(addr: PhysAddr) -> Option<Leaf> {
    if read_cr4() & CR4_LA57 != 0 {
        return None;
    }
    leaf_in(&mut Live, addr.get())
}

/// Calls `f` with every present leaf of the tree under `table` at `level`.
fn each_leaf(tables: &mut impl Tables, table: u64, level: Level, f: &mut impl FnMut(Level, u64)) {
    for i in 0..ENTRIES {
        // SAFETY: `table` is a page table reached from CR3 through present entries.
        let entry = unsafe { (*tables.table(table))[i] };
        if entry & PRESENT == 0 {
            continue;
        }
        match level.below() {
            Some(below) if !level.is_leaf(entry) => each_leaf(tables, entry & ADDR, below, f),
            _ => f(level, entry),
        }
    }
}

#[derive(Debug, Snafu)]
pub enum PagingError {
    #[snafu(display("5-level paging is not handled"))]
    FiveLevel,
    #[snafu(display("no PAT entry is WC and entry 7 is in use"))]
    NoPatSlot,
    #[snafu(display("{addr:#x} is not mapped"))]
    NotMapped { addr: u64 },
    #[snafu(display("allocating a page table: {source}"))]
    Allocate { source: uefi::Error },
}

/// Changes made to the tables, to be undone in reverse.
#[derive(Default)]
struct Undo {
    /// Entries as they were before being overwritten, in the order written.
    entries: Vec<(*mut u64, u64)>,
    /// Tables taken from the spares for splits.
    used: usize,
}

/// Write `value` into `slot`, remembering the old value.
///
/// # Safety
/// `slot` is a live page-table entry and writes to it are allowed (WP clear for read-only tables).
unsafe fn set(undo: &mut Undo, slot: *mut u64, value: u64) {
    // SAFETY: the caller's.
    unsafe {
        undo.entries.push((slot, *slot));
        *slot = value;
    }
}

/// Give every 4 KiB page of `[start, end)` PAT index `index`, splitting large pages that straddle
/// either end with tables from `spares`.
///
/// # Safety
/// As for [`remap`].
unsafe fn retype(
    tables: &mut impl Tables,
    start: u64,
    end: u64,
    index: u8,
    spares: &[u64],
    undo: &mut Undo,
) -> Result<(), PagingError> {
    // SAFETY: the caller's.
    unsafe { remap(tables, start, end, spares, undo, |level, entry| level.with_pat_index(entry, index)) }
}

/// Rewrite every leaf covering `[start, end)` with `leaf(level, entry)`, splitting large pages that
/// straddle either end with tables from `spares`.
///
/// # Safety
/// The tables are live or fake, writes to them are allowed, and nothing reads them concurrently.
unsafe fn remap(
    tables: &mut impl Tables,
    start: u64,
    end: u64,
    spares: &[u64],
    undo: &mut Undo,
    leaf: impl Fn(Level, u64) -> u64,
) -> Result<(), PagingError> {
    let mut addr = start;
    while addr < end {
        let mut table = tables.root();
        let mut level = Level::Pml4;
        loop {
            let slot = tables.table(table).cast::<u64>().wrapping_add(level.index(addr));
            // SAFETY: `slot` is an entry of a table reached through present entries.
            let entry = unsafe { *slot };
            ensure!(entry & PRESENT != 0, NotMappedSnafu { addr });
            if !level.is_leaf(entry) {
                table = entry & ADDR;
                level = level.below().ok_or(PagingError::NotMapped { addr })?;
                continue;
            }
            let base = addr & !(level.size() - 1);
            if base >= start && base + level.size() <= end {
                // SAFETY: the caller's.
                unsafe { set(undo, slot, leaf(level, entry)) };
                addr = base + level.size();
                break;
            }
            let below = level.below().ok_or(PagingError::NotMapped { addr })?;
            let Some(&fresh) = spares.get(undo.used) else { return Err(PagingError::NotMapped { addr }) };
            undo.used += 1;
            let child = tables.table(fresh);
            let frame = level.frame(entry);
            // The large page's flags, with its PAT bit moved to where the level below keeps it.
            let flags = below.with_pat_index(entry & !(ADDR_LARGE | PS | PAT_LARGE), level.pat_index(entry));
            for i in 0..ENTRIES {
                let leaf = frame + i as u64 * below.size();
                let value = if matches!(below, Level::Pt) { leaf | flags } else { leaf | flags | PS };
                // SAFETY: `fresh` is an unused page of ours, the size of a table.
                unsafe { (*child)[i] = value };
            }
            // The new table inherits the large page's protections through its leaves.
            let pointer = fresh | PRESENT | WRITABLE | (entry & (1 << 2));
            // SAFETY: the caller's.
            unsafe { set(undo, slot, pointer) };
            table = fresh;
            level = below;
        }
    }
    Ok(())
}

/// A range remapped write-combining; the tables, the PAT and the spare pages go back on drop.
#[must_use = "dropping it restores the original memory type"]
pub struct WriteCombining {
    undo: Undo,
    spares: Vec<NonNull<u8>>,
    /// The PAT as it was, when an entry had to be repurposed.
    saved_pat: Option<Pat>,
    pat_index: u8,
}

impl fmt::Debug for WriteCombining {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WriteCombining")
            .field("entries", &self.undo.entries.len())
            .field("splits", &self.undo.used)
            .field("pat_index", &self.pat_index)
            .finish_non_exhaustive()
    }
}

impl WriteCombining {
    /// Remap `[base, base + len)` write-combining in the firmware's identity map.
    ///
    /// Uses a PAT entry already WC, or else repurposes entry 7 (UC by default) when no page uses
    /// it. Only this CPU's PAT changes, so only this CPU may touch the range afterwards.
    pub fn map(base: PhysAddr, len: u64) -> Result<Self, PagingError> {
        ensure!(read_cr4() & CR4_LA57 == 0, FiveLevelSnafu);
        let pat = Pat::read();
        let (pat_index, saved_pat) = if let Some(i) = pat.find(MemType::WriteCombining) {
            (i, None)
        } else {
            let mut used = false;
            each_leaf(&mut Live, Live.root(), Level::Pml4, &mut |level, entry| {
                used |= level.pat_index(entry) == 7;
            });
            ensure!(!used, NoPatSlotSnafu);
            (7, Some(pat))
        };
        let spares: Vec<NonNull<u8>> = (0..MAX_SPLITS)
            .map(|_| boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, 1))
            .collect::<Result<_, _>>()
            .map_err(|source| PagingError::Allocate { source })?;
        let spare_addrs: Vec<u64> = spares.iter().map(|p| p.as_ptr() as u64).collect();
        let start = base.get() & !(PAGE - 1);
        let end = (base.get() + len).next_multiple_of(PAGE);
        let mut undo = Undo::default();
        let result = with_tables_writable(|| {
            if let Some(old) = saved_pat {
                // SAFETY: entry 7 is used by no page, so retyping it changes no existing mapping.
                unsafe { write_msr(IA32_PAT, old.with(7, MemType::WriteCombining).0) };
            }
            // SAFETY: interrupts are off and WP is clear; the spares are ours and unused.
            let result = unsafe { retype(&mut Live, start, end, pat_index, &spare_addrs, &mut undo) };
            flush_tlb();
            result
        });
        let mapped = Self { undo, spares, saved_pat, pat_index };
        result.map(|()| mapped)
    }
}

impl Drop for WriteCombining {
    fn drop(&mut self) {
        with_tables_writable(|| {
            for &(slot, old) in self.undo.entries.iter().rev() {
                // SAFETY: the entry was ours to write when mapped and the tables have not been freed.
                unsafe { *slot = old };
            }
            if let Some(pat) = self.saved_pat {
                // SAFETY: the PAT as it was before the map.
                unsafe { write_msr(IA32_PAT, pat.0) };
            }
            flush_tlb();
        });
        for page in self.spares.drain(..) {
            // SAFETY: allocated as one page in `map`; no entry points at it any more.
            if let Err(e) = unsafe { boot::free_pages(page, 1) } {
                log::warn!("paging: freeing a page-table page failed: {e}");
            }
        }
    }
}

/// One 4 KiB page made not-present in the firmware's identity map, so any access to it faults; present
/// again on drop. Used as a guard below a stack.
///
/// A large page around it is split for good: the tables stay, since other guards may sit in them. Only
/// this CPU's TLB is flushed, so only this CPU may run on the stack above it.
#[must_use = "dropping it makes the page present again"]
pub struct GuardPage {
    slot: NonNull<u64>,
    entry: u64,
}

impl fmt::Debug for GuardPage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GuardPage")
            .field("slot", &self.slot)
            .field("entry", &format_args!("{:#x}", self.entry))
            .finish()
    }
}

/// Splits one page can need: a 1 GiB page into 2 MiB ones, then one of those into 4 KiB ones.
const GUARD_SPLITS: usize = 2;

impl GuardPage {
    /// Make the page at `page` (4 KiB aligned) not-present.
    ///
    /// # Safety
    /// The page is this program's and nothing accesses it while the guard lives.
    pub unsafe fn new(page: PhysAddr) -> Result<Self, PagingError> {
        ensure!(read_cr4() & CR4_LA57 == 0, FiveLevelSnafu);
        let spares: Vec<NonNull<u8>> = (0..GUARD_SPLITS)
            .map(|_| boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, 1))
            .collect::<Result<_, _>>()
            .map_err(|source| PagingError::Allocate { source })?;
        let spare_addrs: Vec<u64> = spares.iter().map(|p| p.as_ptr() as u64).collect();
        let start = page.get() & !(PAGE - 1);
        let mut undo = Undo::default();
        let result = with_tables_writable(|| {
            // SAFETY: interrupts are off and WP is clear; the spares are ours and unused; the page is
            // the caller's to fault.
            let result = unsafe { remap(&mut Live, start, start + PAGE, &spare_addrs, &mut undo, |_, e| e & !PRESENT) };
            flush_tlb();
            result
        });
        // Tables a split took now hold live entries; the spares a split did not take go back.
        for page in spares.into_iter().skip(undo.used) {
            // SAFETY: allocated as one page above, and no entry points at it.
            if let Err(e) = unsafe { boot::free_pages(page, 1) } {
                log::warn!("paging: freeing a spare page-table page failed: {e}");
            }
        }
        result?;
        let &(slot, entry) = undo.entries.last().ok_or(PagingError::NotMapped { addr: start })?;
        let slot = NonNull::new(slot).ok_or(PagingError::NotMapped { addr: start })?;
        Ok(Self { slot, entry })
    }
}

impl Drop for GuardPage {
    fn drop(&mut self) {
        with_tables_writable(|| {
            // SAFETY: the leaf entry this guard cleared; splits are never undone, so it is still live.
            unsafe { self.slot.as_ptr().write(self.entry) };
            flush_tlb();
        });
    }
}

/// Run `f` with interrupts off and CR0.WP clear, since firmware may map its tables read-only.
fn with_tables_writable<R>(f: impl FnOnce() -> R) -> R {
    without_interrupts(|| {
        let cr0 = read_cr0();
        // SAFETY: clearing WP only lets CPL 0 write read-only pages; it is restored below.
        unsafe { write_cr0(cr0 & !CR0_WP) };
        let result = f();
        // SAFETY: the value read above.
        unsafe { write_cr0(cr0) };
        result
    })
}

/// Drop every translation, global ones included.
fn flush_tlb() {
    let cr4 = read_cr4();
    if cr4 & CR4_PGE != 0 {
        // SAFETY: toggling PGE flushes the TLB and changes nothing else.
        unsafe {
            write_cr4(cr4 & !CR4_PGE);
            write_cr4(cr4);
        }
    } else {
        // SAFETY: reloading CR3 with its own value flushes non-global translations.
        unsafe { core::arch::asm!("mov cr3, {}", in(reg) read_cr3(), options(nostack, preserves_flags)) };
    }
}

#[cfg(test)]
mod tests;
