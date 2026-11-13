//! Turning a static-PIE ELF built for `x86_64-unknown-linux-gnu` into a PE32+ UEFI application.
//!
//! Apps are built as ELF without libc (the flags of `.cargo/app.toml`) because that target has SSE and
//! AVX, where `x86_64-unknown-uefi` is soft float, and because the Linux C is ELF with the System V ABI,
//! which the PE linker of `x86_64-unknown-uefi` does not take. The linked ELF is turned into the image the
//! firmware loads here, as gnu-efi and systemd do with their `elf2efi`.
//!
//! Each `PT_LOAD` segment becomes one PE section on the same pages. The whole image moves up by the
//! page or pages the PE headers take, which RIP-relative code does not notice; the absolute pointers
//! are exactly the `R_X86_64_RELATIVE` slots, which are written for the preferred base and listed as
//! `DIR64` base relocations so the firmware can load the image anywhere. Any other dynamic relocation,
//! a TLS segment or an interpreter is refused rather than half converted.
//!
//! An address in the PE image is the ELF address plus [`Plan::shift`], so the ELF the build leaves
//! next to the `.efi` still resolves addresses from a fault or a backtrace.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use object::elf;
use object::pe;
use object::read::elf::{Dyn, FileHeader, ProgramHeader, Rela};
use object::write::pe::{NtHeaders, Writer};
use snafu::{OptionExt, ResultExt, Snafu};

type ElfHeader = elf::FileHeader64<object::LittleEndian>;

const PAGE: u64 = 0x1000;
const FILE_ALIGNMENT: u32 = 0x200;
/// The preferred load address. The firmware relocates the image wherever it puts it.
const IMAGE_BASE: u64 = 0x1_4000_0000;

#[derive(Debug, Snafu)]
pub enum EfiImageError {
    #[snafu(display("not a readable 64-bit ELF"))]
    Parse { source: object::Error },

    #[snafu(display(
        "expected an x86-64 position-independent executable (ET_DYN), found machine {machine} type {kind}"
    ))]
    NotPie { machine: u16, kind: u16 },

    #[snafu(display("the ELF has no loadable segments"))]
    NoSegments,

    #[snafu(display("the ELF has a {what}, which a UEFI image cannot carry"))]
    Unsupported { what: &'static str },

    #[snafu(display("segment data at {vaddr:#x} lies outside the file"))]
    SegmentData { vaddr: u64 },

    #[snafu(display("segments at {first:#x} and {second:#x} share a page"))]
    SharedPage { first: u64, second: u64 },

    #[snafu(display("relocation {index} has type {kind}; only R_X86_64_RELATIVE is supported"))]
    RelocationType { index: usize, kind: u32 },

    #[snafu(display("relocation slot {offset:#x} is not in a segment's file data"))]
    RelocationSlot { offset: u64 },

    #[snafu(display("the relocation table at {vaddr:#x} ({size} bytes) is not in a segment's file data"))]
    RelocationTable { vaddr: u64, size: u64 },

    #[snafu(display("the image is larger than PE32+ allows"))]
    TooLarge,

    #[snafu(display("writing the PE image failed"))]
    Write { source: object::write::Error },

    #[snafu(display("cannot read {}", path.display()))]
    ReadFile { path: PathBuf, source: io::Error },

    #[snafu(display("cannot write {}", path.display()))]
    WriteFile { path: PathBuf, source: io::Error },
}

/// Access rights of a loadable segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Read,
    ReadExecute,
    ReadWrite,
}

/// One `PT_LOAD` segment with its file bytes.
#[derive(Clone, Debug)]
pub struct Segment {
    pub vaddr: u64,
    pub mem_size: u64,
    pub access: Access,
    pub data: Vec<u8>,
}

/// One `R_X86_64_RELATIVE`: the pointer at `offset` is the load base plus `addend`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Relative {
    pub offset: u64,
    pub addend: i64,
}

/// The image laid out at its preferred base, before it is serialized.
#[derive(Debug)]
pub struct Plan {
    /// What is added to every ELF address to get the PE relative address.
    pub shift: u64,
    /// Segments in address order, with every relocation slot already written.
    pub segments: Vec<Segment>,
    /// Relative addresses of the 64-bit pointers the firmware rebases, ascending.
    pub fixups: Vec<u32>,
    pub entry: u32,
}

/// Convert the ELF at `elf` into the EFI application `efi`.
pub fn convert_file(elf: &Path, efi: &Path) -> Result<(), EfiImageError> {
    let bytes = fs::read(elf).context(ReadFileSnafu { path: elf })?;
    let image = convert(&bytes)?;
    fs::write(efi, image).context(WriteFileSnafu { path: efi })
}

/// Convert a static-PIE ELF into a PE32+ EFI application.
pub fn convert(elf_bytes: &[u8]) -> Result<Vec<u8>, EfiImageError> {
    let (segments, relatives, entry) = read_elf(elf_bytes)?;
    let plan = plan(segments, &relatives, entry)?;
    write_pe(&plan)
}

/// Place the segments above the headers' page and apply each relocation for [`IMAGE_BASE`].
pub fn plan(mut segments: Vec<Segment>, relatives: &[Relative], entry: u64) -> Result<Plan, EfiImageError> {
    segments.sort_by_key(|s| s.vaddr);
    let first = segments.first().context(NoSegmentsSnafu)?;
    for pair in segments.windows(2) {
        let end = pair[0].vaddr + pair[0].mem_size;
        if end.div_ceil(PAGE) > pair[1].vaddr / PAGE {
            return Err(EfiImageError::SharedPage { first: pair[0].vaddr, second: pair[1].vaddr });
        }
    }
    // The headers take the page below the first segment's page.
    let shift = PAGE - (first.vaddr & !(PAGE - 1));

    let mut fixups = Vec::with_capacity(relatives.len());
    for r in relatives {
        let (segment, at) = file_offset(&segments, r.offset, 8).context(RelocationSlotSnafu { offset: r.offset })?;
        let value = r.addend.cast_unsigned().wrapping_add(shift + IMAGE_BASE);
        segments[segment].data[at..at + 8].copy_from_slice(&value.to_le_bytes());
        fixups.push(rva(r.offset + shift)?);
    }
    fixups.sort_unstable();
    Ok(Plan { shift, segments, fixups, entry: rva(entry + shift)? })
}

fn rva(address: u64) -> Result<u32, EfiImageError> {
    u32::try_from(address).ok().context(TooLargeSnafu)
}

/// The segment holding `len` file bytes at `vaddr`, and their offset in its data.
fn file_offset(segments: &[Segment], vaddr: u64, len: u64) -> Option<(usize, usize)> {
    segments.iter().enumerate().find_map(|(i, s)| {
        let in_file = vaddr >= s.vaddr && vaddr + len <= s.vaddr + s.data.len() as u64;
        in_file.then(|| (i, (vaddr - s.vaddr) as usize))
    })
}

type ElfContents = (Vec<Segment>, Vec<Relative>, u64);

fn read_elf(bytes: &[u8]) -> Result<ElfContents, EfiImageError> {
    let header = ElfHeader::parse(bytes).context(ParseSnafu)?;
    let e = header.endian().context(ParseSnafu)?;
    let (machine, kind) = (header.e_machine(e), header.e_type(e));
    if machine != elf::EM_X86_64 || kind != elf::ET_DYN {
        return Err(EfiImageError::NotPie { machine: machine.0, kind: kind.0 });
    }
    let mut segments = Vec::new();
    let mut table = None;
    for ph in header.program_headers(e, bytes).context(ParseSnafu)? {
        match ph.p_type(e) {
            elf::PT_LOAD => {
                let vaddr = ph.p_vaddr(e);
                let data = ph.data(e, bytes).ok().context(SegmentDataSnafu { vaddr })?.to_vec();
                let flags = ph.p_flags(e).0;
                let access = if flags & elf::PF_X.0 != 0 {
                    Access::ReadExecute
                } else if flags & elf::PF_W.0 != 0 {
                    Access::ReadWrite
                } else {
                    Access::Read
                };
                segments.push(Segment { vaddr, mem_size: ph.p_memsz(e), access, data });
            },
            elf::PT_DYNAMIC => {
                let entries = ph.dynamic(e, bytes).context(ParseSnafu)?.unwrap_or_default();
                table = Some(rela_table(entries, e)?);
            },
            elf::PT_TLS => return Err(EfiImageError::Unsupported { what: "TLS segment" }),
            elf::PT_INTERP => return Err(EfiImageError::Unsupported { what: "program interpreter" }),
            _ => {},
        }
    }
    let relatives = match table {
        Some((vaddr, size)) if size > 0 => relatives(&segments, vaddr, size, e)?,
        _ => Vec::new(),
    };
    Ok((segments, relatives, header.e_entry(e)))
}

/// Address and size of `DT_RELA`; the other relocation tables are refused.
fn rela_table(
    entries: &[elf::Dyn64<object::LittleEndian>],
    e: object::LittleEndian,
) -> Result<(u64, u64), EfiImageError> {
    let (mut vaddr, mut size) = (0, 0);
    for entry in entries {
        match entry.d_tag(e) {
            elf::DT_RELA => vaddr = entry.d_val(e),
            elf::DT_RELASZ => size = entry.d_val(e),
            elf::DT_REL | elf::DT_RELR => {
                return Err(EfiImageError::Unsupported { what: "REL or RELR relocation table" });
            },
            elf::DT_JMPREL => return Err(EfiImageError::Unsupported { what: "PLT relocation table" }),
            elf::DT_NEEDED => return Err(EfiImageError::Unsupported { what: "shared library dependency" }),
            _ => {},
        }
    }
    Ok((vaddr, size))
}

fn relatives(
    segments: &[Segment],
    vaddr: u64,
    size: u64,
    e: object::LittleEndian,
) -> Result<Vec<Relative>, EfiImageError> {
    let (segment, at) = file_offset(segments, vaddr, size).context(RelocationTableSnafu { vaddr, size })?;
    let raw = &segments[segment].data[at..at + size as usize];
    let entries: &[elf::Rela64<object::LittleEndian>] =
        object::pod::slice_from_all_bytes(raw).ok().context(RelocationTableSnafu { vaddr, size })?;
    entries
        .iter()
        .enumerate()
        .map(|(index, r)| {
            let kind = r.r_type(e, false);
            if kind != elf::R_X86_64_RELATIVE {
                return Err(EfiImageError::RelocationType { index, kind: kind.0 });
            }
            Ok(Relative { offset: r.r_offset(e), addend: r.r_addend(e) })
        })
        .collect()
}

fn section_header(access: Access, repeat: usize) -> ([u8; 8], pe::SectionFlags) {
    let (base, flags): (&[u8], _) = match access {
        Access::Read => (b".rdata", pe::IMAGE_SCN_CNT_INITIALIZED_DATA | pe::IMAGE_SCN_MEM_READ),
        Access::ReadExecute => (b".text", pe::IMAGE_SCN_CNT_CODE | pe::IMAGE_SCN_MEM_EXECUTE | pe::IMAGE_SCN_MEM_READ),
        Access::ReadWrite => {
            (b".data", pe::IMAGE_SCN_CNT_INITIALIZED_DATA | pe::IMAGE_SCN_MEM_READ | pe::IMAGE_SCN_MEM_WRITE)
        },
    };
    let mut name = [0; 8];
    name[..base.len()].copy_from_slice(base);
    if repeat > 0 {
        name[base.len()] = b'0' + (repeat as u8).min(9);
    }
    (name, flags)
}

fn write_pe(plan: &Plan) -> Result<Vec<u8>, EfiImageError> {
    let mut out = Vec::new();
    let mut w = Writer::new(true, PAGE as u32, FILE_ALIGNMENT, &mut out);
    w.reserve_dos_header_and_stub();
    w.reserve_nt_headers(pe::IMAGE_NUMBEROF_DIRECTORY_ENTRIES);
    let sections = u16::try_from(plan.segments.len() + 1).ok().context(TooLargeSnafu)?;
    w.reserve_section_headers(sections);

    let mut placed = Vec::with_capacity(plan.segments.len());
    let mut seen = [0; 3];
    for s in &plan.segments {
        let start = rva((s.vaddr & !(PAGE - 1)) + plan.shift)?;
        let lead = (s.vaddr & (PAGE - 1)) as u32;
        let kind = s.access as usize;
        let (name, flags) = section_header(s.access, seen[kind]);
        seen[kind] += 1;
        w.reserve_virtual_until(start);
        let virtual_size = lead + rva(s.mem_size)?;
        let file_size = lead + rva(s.data.len() as u64)?;
        let range = w.reserve_section(name, flags, virtual_size, file_size);
        placed.push((range.file_offset, lead));
    }
    for &fixup in &plan.fixups {
        w.add_reloc(fixup, pe::IMAGE_REL_BASED_DIR64);
    }
    w.reserve_reloc_section();

    w.write_dos_header_and_stub().context(WriteSnafu)?;
    w.write_nt_headers(NtHeaders {
        machine: pe::IMAGE_FILE_MACHINE_AMD64,
        time_date_stamp: 0,
        characteristics: pe::IMAGE_FILE_EXECUTABLE_IMAGE | pe::IMAGE_FILE_LARGE_ADDRESS_AWARE,
        major_linker_version: 0,
        minor_linker_version: 0,
        address_of_entry_point: plan.entry,
        image_base: IMAGE_BASE,
        major_operating_system_version: 0,
        minor_operating_system_version: 0,
        major_image_version: 0,
        minor_image_version: 0,
        major_subsystem_version: 0,
        minor_subsystem_version: 0,
        subsystem: pe::IMAGE_SUBSYSTEM_EFI_APPLICATION,
        dll_characteristics: pe::IMAGE_DLLCHARACTERISTICS_DYNAMIC_BASE
            | pe::IMAGE_DLLCHARACTERISTICS_HIGH_ENTROPY_VA
            | pe::IMAGE_DLLCHARACTERISTICS_NX_COMPAT,
        size_of_stack_reserve: 0,
        size_of_stack_commit: 0,
        size_of_heap_reserve: 0,
        size_of_heap_commit: 0,
    });
    w.write_section_headers();
    for (s, &(file_offset, lead)) in plan.segments.iter().zip(&placed) {
        let mut data = vec![0; lead as usize];
        data.extend_from_slice(&s.data);
        w.write_section(file_offset, &data);
    }
    w.write_reloc_section();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};

    use super::*;

    fn segment(vaddr: u64, mem_size: u64, access: Access, file_len: usize) -> Segment {
        Segment { vaddr, mem_size, access, data: vec![0; file_len] }
    }

    /// The layout of a real static-PIE app link: read-only data from 0, then text, relro and data.
    fn linked() -> Vec<Segment> {
        vec![
            segment(0x2cb0, 0x3665, Access::ReadExecute, 0x3665),
            segment(0, 0x1cb0, Access::Read, 0x1cb0),
            segment(0x7318, 0xce8, Access::ReadWrite, 0x738),
            segment(0x8a50, 0x38, Access::ReadWrite, 0x10),
        ]
    }

    #[test]
    fn relative_slots_hold_the_preferred_address_and_become_base_relocations() {
        let relatives = [Relative { offset: 0x8a58, addend: 0x10 }, Relative { offset: 0x7320, addend: 0x2d20 }];
        assert!(let Ok(plan) = plan(linked(), &relatives, 0x2d20));
        check!(plan.shift == 0x1000);
        check!(plan.entry == 0x3d20);
        check!(plan.fixups == [0x8320, 0x9a58]);
        let data = &plan.segments[2].data;
        check!(data[8..16] == (IMAGE_BASE + 0x1000 + 0x2d20).to_le_bytes());
        check!(plan.segments[3].data[8..16] == (IMAGE_BASE + 0x1010).to_le_bytes());
    }

    #[test]
    fn a_slot_in_zero_filled_memory_is_refused() {
        // 0x7318 + 0x738 is where the segment's file bytes end; past that is only bss.
        let relatives = [Relative { offset: 0x7318 + 0x738, addend: 0 }];
        assert!(let Err(EfiImageError::RelocationSlot { offset: 0x7a50 }) = plan(linked(), &relatives, 0));
    }

    #[test]
    fn segments_sharing_a_page_are_refused() {
        let segments = vec![segment(0, 0x1800, Access::Read, 0x1800), segment(0x1900, 0x10, Access::ReadExecute, 0x10)];
        assert!(let Err(EfiImageError::SharedPage { first: 0, second: 0x1900 }) = plan(segments, &[], 0x1900));
    }

    #[test]
    fn a_non_relative_relocation_is_refused() {
        let mut bytes = vec![0; 24];
        bytes[0..8].copy_from_slice(&0x10u64.to_le_bytes());
        bytes[8..16].copy_from_slice(&u64::from(elf::R_X86_64_64.0).to_le_bytes());
        let segments = [Segment { vaddr: 0, mem_size: 24, access: Access::Read, data: bytes }];
        let result = relatives(&segments, 0, 24, object::LittleEndian);
        assert!(let Err(EfiImageError::RelocationType { index: 0, kind: 1 }) = result);
    }
}
