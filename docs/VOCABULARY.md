# Vocabulary

Canonical domain terminology for ferrite. All code, docs, comments, and output should use these terms consistently. See the vocabulary audit issue for current code locations that deviate.

---

## Memory Hardware

### stuck bit
A DRAM cell permanently stuck at 0 (stuck low) or 1 (stuck high), returning the same value regardless of what was written. Detected by patterns that write both values to every bit position: Solid Bits, Walking Ones/Zeros, Checkerboard.

Code: `ErrorClassification::StuckBit`

**Avoid:** "stuck address" to mean a stuck bit. Stuck Address is a pattern name for a different fault — address aliasing.

### address aliasing
A fault where two or more addresses resolve to the same underlying memory cell. Writes through one address silently overwrite the value at another. Detected by the Stuck Address pattern, which writes each word's index as its value and verifies that no overwrites occurred.

Code: `Pattern::StuckAddress`

**Avoid:** "stuck address error" — address aliasing is the fault being detected, not a type of stuck bit.

### huge page (HugeTLB)
An explicit 2 MiB or 1 GiB page allocated through the HugeTLB kernel interface (`MAP_HUGETLB`, hugetlbfs). Applications must request these explicitly. Tracked in `MapStats.huge_pages`.

**Avoid:** "huge page" as a generic term for any large page. THP and HugeTLB huge pages are distinct mechanisms.

### THP (Transparent Huge Page)
A 2 MiB page that the kernel allocates and promotes transparently, without an explicit application request. The kernel collapses clusters of 4 KiB pages into a THP when conditions allow. Tracked in `MapStats.thp_pages`.

**Avoid:** "THP" to mean all large pages, or "huge page" to mean THP.

---

## EDAC and DIMMs

### DIMM
The physical memory module. Canonical user-facing term for any identified memory module, regardless of which EDAC API path (modern dimm-based or legacy csrow-based) was used to read it. EDAC labels (e.g., `DIMM_A1`) are surfaced directly in output.

JSON: `label`, `mc`, `dimm_index` fields in `ecc_deltas` events.

**Avoid:** "CSROW" in user-facing output or documentation.

### memory controller (MC)
The hardware component managing communication between the CPU and DIMMs. In EDAC sysfs: `mc0`, `mc1`, etc. Field: `mc` on `DimmEdac` and `EccDelta`.

Not to be confused with ferrite's allocation or segment concepts.

### CSROW
A Linux EDAC kernel concept: one row in the legacy EDAC sysfs hierarchy (`mc0/csrow0/`). Internal implementation detail used in `try_read_csrow_api()`. Not for user-facing output or documentation. Use DIMM instead.

### correctable error (CE)
An ECC single-bit error the memory controller detected and corrected automatically. Nonzero CE counts may indicate degradation. Field: `ce_delta` in `EccDelta`. Abbreviation CE is acceptable.

### uncorrectable error (UE)
An ECC multi-bit error the memory controller could not correct. Indicates likely data corruption. Field: `ue_delta` in `EccDelta`. Abbreviation UE is acceptable.

**Note:** "error" in CE/UE terminology is standard ECC hardware vocabulary and is an intentional exception to the rule that "error" should not be used for memory test faults. See `failure` below.

---

## Allocation and Testing

### allocation
The full anonymous memory block that ferrite mmap's and mlock's before testing. The entire test target. Size set by `--size`.

Code type: `TestBuffer`.

**Avoid:** "region" as a domain term. The allocation is tested as a single whole by one `runner::run()` call; parallelism (`--parallel <N|auto>`) happens inside the pattern loop via Rayon, not by splitting the allocation into worker-owned pieces. `alloc.rs`'s low-level mmap/mlock wrapper is `TestBuffer`, not "region" -- use that name.

### segment
The TUI's display unit for the allocation. Since the allocation is tested as a whole, there is exactly one segment per run; it tracks pattern progress, activity, and failures for rendering the heatmap and header.

Code type: `Segment`.

### word
A 64-bit unsigned integer (`u64`). The fundamental unit of test data in ferrite. All patterns operate on word-aligned buffers. A `Failure` identifies one mismatched word.

**Avoid:** "byte" — ferrite does not operate at byte granularity.

---

## Test Execution

### pattern
A named memory test algorithm. Each pattern fills the allocation with a specific bit pattern and reads it back to detect faults. Available patterns: `Solid Bits`, `Walking Ones`, `Walking Zeros`, `Checkerboard`, `Stuck Address`, `March C-`.

CLI: `--test <pattern-name>`. Code: `Pattern` enum, `run_pattern()`.

**Avoid:** "test" as a noun to mean a pattern. A pattern is the algorithm; `--test` is CLI ergonomics.

### sub-pass
One internal fill-and-verify cycle within a pattern. `Pattern::sub_passes()` returns the count per pattern — for example, Solid Bits has 2 sub-passes (all-ones then all-zeros), Walking Ones has 64 (one per bit position).

JSON fields: `sub_pass`, `total_sub_passes` in `progress` events. In prose: always hyphenated ("sub-pass", not "subpass").

### pass
One iteration of all selected patterns over the full allocation. `--passes N` runs N consecutive passes. Each pass produces a `PassResult`.

**Avoid:** "run" to mean a single pass.

### run
The complete ferrite execution from start to final result, spanning all passes. JSON event: `run_summary`.

**Avoid:** "run" to mean a single pass.

### failure
A single 64-bit word that read back a different value than was written. The fundamental unit of a memory fault in ferrite.

Code: `Failure` struct, `Vec<Failure>`. JSON: `failures` array in `test_fail` events; `failures` count in `pass_complete`.

**Avoid:** "error" to mean a memory failure. "error" is reserved for operational failures (I/O errors, program errors).

---

## Addressing

### virtual address
The address the process sees: a pointer into the process's virtual address space. All ferrite buffer accesses use virtual addresses.

### physical address
The absolute hardware address of a memory cell, as seen by the memory controller. Resolved from virtual addresses via `/proc/self/pagemap`. Represented by the `PhysAddr` newtype. JSON field: `phys_addr` in failure records within `test_fail` events.

**Avoid:** "PFN" to mean a full physical address.

### PFN (page frame number)
Physical address >> 12. The page-granular index used in kernel pagemap interfaces (`/proc/self/pagemap`, `/proc/kpageflags`). Represented by the `Pfn` newtype (distinct from `PhysAddr` -- see `frame` below); convert between the two only via `Pfn::to_addr()` / `Pfn::from_addr()`. `PhysAddr::pfn()` returns the bare `u64` index for call sites that don't need the newtype.

A PFN identifies a 4 KiB page, not a byte. It is not interchangeable with a physical address.

**Avoid:** using PFN where a byte-granular physical address is meant.

### frame
A `Pfn`-indexed 4 KiB page frame -- the unit of tracking for physical coverage, gap classification, and culling. "Frame" and "PFN" are used interchangeably when talking about a single page-granular unit; `frame` reads more naturally in prose about coverage sets ("untested frames"), `PFN` when citing the concrete index or kernel interface.

Code: `physmem::pfn::Pfn`, `physmem::pfn::PfnRange` (a compact range of frames; coverage sets, gap reports, and cull targets are all `Vec<PfnRange>`).

### physical coverage
The fraction of installed physical RAM a run actually tested: tested bytes / installed RAM. Tested bytes is the numerator — pages that resolved to a real PFN × 4 KiB (`MapStats::resolved_pages`, `MapStats::tested_bytes()`). Reported as the `coverage` object (`sysmem::Coverage`, `status: measured | unavailable`) in the results document and the `run_complete` NDJSON event. Unavailable without physical resolution (`--no-phys` or missing `CAP_SYS_ADMIN`).

**Avoid:** conflating single-run coverage (this measurement) with cross-run coverage tracking (persisting tested PFNs across runs — a separate, future concern).

### installed RAM
Total testable physical memory — the coverage denominator. Summed from `/proc/iomem` "System RAM" ranges when readable as root, falling back to `/proc/meminfo` `MemTotal` otherwise (a slight underestimate, since `MemTotal` excludes firmware/kernel-reserved regions). Type: `sysmem::InstalledRam`, tagged with its `RamSource`.

**Avoid:** "total memory" ambiguously — distinguish installed RAM (denominator) from the run size (the allocation) and from tested bytes (the numerator).

---

## Physical Coverage and /dev/mem

### hostage / frame-hostage culling
A frame held mapped and mlocked, purely to prevent the kernel from handing it back out, is a hostage. Frame-hostage culling (`--cull`) sweeps available RAM before the real test allocation, holds every already-covered frame hostage, and releases the rest back to the kernel -- forcing the buddy allocator to serve the test buffer fresh, untested frames instead of re-handing out the same ones. Requires `--coverage-file` (there's nothing to steer toward without a covered set).

Code: `physmem::sieve::FrameSieve`. See `docs/COVERAGE.md` Tier 3.

**Avoid:** "hostage" for the test buffer itself -- the test buffer is the allocation under test, not a hostage; hostages exist only during the pre-allocation sieve and are released once the test buffer is locked.

### sieve
The mechanism that performs frame-hostage culling: sweeps available memory in 2 MiB (THP) blocks, resolves each block's physical frames, and decides hostage-or-release per block based on whether it's already covered.

Code: `physmem::sieve::FrameSieve`.

### gap
The untested remainder of installed RAM after subtracting covered frames -- what physical coverage doesn't say anything about on its own. A gap report classifies every gap frame by `FrameClass` so "untested" reads as "acquirable" / "reclaimable" / "in use" / "unreachable" rather than an undifferentiated shortfall.

Code: `physmem::gap::GapReport`. See `docs/COVERAGE.md` ("Honest denominators").

### FrameClass
What an untested frame is doing right now, and therefore whether a future run could reach it, per `/proc/kpageflags`:

- `Free` — in the buddy allocator, acquirable immediately.
- `Reclaimable` — file-backed page cache, acquirable under allocation pressure.
- `InUse` — anonymous or shmem/tmpfs memory held by another process, freed by stopping services or rebooting.
- `Unreachable` — kernel text/data, slab, page tables, reserved, poisoned, or offline; not reachable from userspace by design.

Code: `physmem::gap::FrameClass`.

### probe
A read-only check of a `/dev/mem` target: map the physical range, read it back, and report without writing. Used for `Safety` tiers where a write test would be unsafe or destructive, or when `--devmem` is given without `--devmem-unsafe` for System RAM.

**Avoid:** "test" for a probe -- a probe never writes, so it cannot detect stuck bits or coupling faults; it only confirms the range is mappable and readable.

### memmap reservation
A physical range the kernel was told to carve out of System RAM at boot via the `memmap=` command-line parameter, and therefore never touches. Parsed from `/proc/cmdline`. Safe to write-test through `/dev/mem` without `--devmem-unsafe`, since the kernel has already excluded the range from its own use.

Code: `physmem::devmem::parse_target` (the `reserved` target selects all such ranges), `Safety::Reserved`.

### Safety (devmem write-safety tiers)
How `--devmem` classifies a physical range before deciding whether to write-test or read-only probe it, cross-referencing `/proc/iomem` and `memmap=` reservations:

- `Reserved` — inside a `memmap=`-reserved region; the kernel doesn't touch it, so writes are safe and allowed by default.
- `SystemRam` — live System RAM the kernel or other processes may be using; writing can crash the machine, so it's only allowed with `--devmem-unsafe`.
- `FirmwareOrMmio` — neither reserved-RAM nor System RAM (ACPI tables, PCI MMIO, firmware); writing can brick hardware, so it is never allowed, even with `--devmem-unsafe`.

Code: `physmem::devmem::Safety`.
