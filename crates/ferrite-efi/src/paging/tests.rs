use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use assert2::check;

use super::*;

/// Page tables in host memory, keyed by the physical address they stand for.
struct Fake {
    root: u64,
    tables: BTreeMap<u64, Box<[u64; ENTRIES]>>,
}

impl Tables for Fake {
    fn root(&self) -> u64 {
        self.root
    }

    fn table(&mut self, phys: u64) -> *mut [u64; ENTRIES] {
        &raw mut **self.tables.entry(phys).or_insert_with(|| Box::new([0; ENTRIES]))
    }
}

const ROOT: u64 = 0x1000;
const PDPT: u64 = 0x2000;
const SPARES: [u64; MAX_SPLITS] = [0x10_0000, 0x10_1000, 0x10_2000, 0x10_3000];
const GIB: u64 = 1 << 30;
/// Flags of the firmware's large pages: present, writable, no-execute, uncacheable (PAT index 3).
const FLAGS: u64 = PRESENT | WRITABLE | PWT | PCD | 1 << 63;

/// The first 8 GiB identity-mapped with 1 GiB pages.
fn identity() -> Fake {
    let mut fake = Fake { root: ROOT, tables: BTreeMap::new() };
    // SAFETY: fake tables in host memory.
    unsafe {
        (*fake.table(ROOT))[0] = PDPT | PRESENT | WRITABLE;
        for i in 0..8 {
            (*fake.table(PDPT))[i] = (i as u64 * GIB) | FLAGS | PS;
        }
    }
    fake
}

fn pat_at(fake: &mut Fake, addr: u64) -> (u64, u8) {
    let leaf = leaf_in(fake, addr).unwrap();
    (leaf.size, leaf.pat_index)
}

#[test]
fn a_framebuffer_inside_a_gib_page_is_retyped_and_nothing_around_it() {
    let mut fake = identity();
    // A 1920x1080 frame 2 MiB into the fourth GiB: the start is 2 MiB aligned, the end is not.
    let start = 3 * GIB + (2 << 20);
    let end = start + 1920 * 1080 * 4;
    let mut undo = Undo::default();
    // SAFETY: fake tables.
    unsafe { retype(&mut fake, start, end.next_multiple_of(PAGE), 1, &SPARES, &mut undo).unwrap() };

    check!(undo.used == 2, "the GiB page and the 2 MiB page holding the end are split");
    check!(pat_at(&mut fake, start) == (2 << 20, 1));
    check!(pat_at(&mut fake, end - 1) == (PAGE, 1));
    check!(pat_at(&mut fake, end.next_multiple_of(PAGE)) == (PAGE, 3), "past the end keeps its type");
    check!(pat_at(&mut fake, start - 1) == (2 << 20, 3));
    check!(pat_at(&mut fake, 2 * GIB) == (GIB, 3), "other GiB pages are untouched");
    check!(pat_at(&mut fake, start + (5 << 20)) == (2 << 20, 1));
}

#[test]
fn split_pages_keep_their_frames_and_flags() {
    let mut fake = identity();
    let addr = 5 * GIB + 0x12_3000;
    let mut undo = Undo::default();
    // SAFETY: fake tables.
    unsafe { retype(&mut fake, addr, addr + PAGE, 1, &SPARES, &mut undo).unwrap() };
    check!(undo.used == 2);
    // SAFETY: fake tables.
    let (pd, pt) = unsafe { ((*fake.table(SPARES[0]))[1], (*fake.table(SPARES[1]))[0x124]) };
    check!(pd & ADDR_LARGE == 5 * GIB + (2 << 20), "the 2 MiB page after the split one");
    check!(pd & (PS | 1 << 63 | WRITABLE) == PS | 1 << 63 | WRITABLE);
    check!(pt & ADDR == 5 * GIB + 0x12_4000, "the neighbour of the retyped page");
    check!(Level::Pt.pat_index(pt) == 3);
}

#[test]
fn undoing_in_reverse_restores_every_entry() {
    let mut fake = identity();
    // SAFETY: fake tables.
    let before: Vec<u64> = (0..8).map(|i| unsafe { (*fake.table(PDPT))[i] }).collect();
    let mut undo = Undo::default();
    // SAFETY: fake tables.
    unsafe { retype(&mut fake, 3 * GIB + 0x1000, 3 * GIB + 0x9000, 1, &SPARES, &mut undo).unwrap() };
    for &(slot, old) in undo.entries.iter().rev() {
        // SAFETY: entries of the fake tables.
        unsafe { *slot = old };
    }
    // SAFETY: fake tables.
    let after: Vec<u64> = (0..8).map(|i| unsafe { (*fake.table(PDPT))[i] }).collect();
    check!(before == after);
}

#[test]
fn an_unmapped_address_fails() {
    let mut fake = identity();
    let mut undo = Undo::default();
    // SAFETY: fake tables.
    let result = unsafe { retype(&mut fake, 600 * GIB, 600 * GIB + PAGE, 1, &SPARES, &mut undo) };
    check!(matches!(result, Err(PagingError::NotMapped { .. })));
}

#[test]
fn pat_entries_round_trip_their_index() {
    for level in [Level::Pdpt, Level::Pd, Level::Pt] {
        for index in 0..8 {
            check!(level.pat_index(level.with_pat_index(FLAGS, index)) == index);
        }
    }
    let default = Pat(0x0007_0406_0007_0406);
    check!(default.find(MemType::WriteCombining).is_none());
    check!(default.entry(7) == MemType::Uncacheable);
    check!(default.with(7, MemType::WriteCombining).entry(7) == MemType::WriteCombining);
}
