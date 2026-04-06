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

**Avoid:** "region" to mean the full allocation. Region is reserved for parallel segments.

### segment
A subdivision of the allocation assigned to one worker thread for concurrent testing. In TUI mode, the allocation is split into N segments (controlled by `--regions`, defaulting to CPU count). Each segment runs all selected patterns independently across its slice of the allocation.

Code type: `Segment`.

### word
A 64-bit unsigned integer (`u64`). The fundamental unit of test data in ferrite. All patterns operate on word-aligned buffers. A `Failure` identifies one mismatched word.

**Avoid:** "byte" — ferrite does not operate at byte granularity.

---

## Test Execution

### pattern
A named memory test algorithm. Each pattern fills the allocation with a specific bit pattern and reads it back to detect faults. Available patterns: `Solid Bits`, `Walking Ones`, `Walking Zeros`, `Checkerboard`, `Stuck Address`.

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
Physical address >> 12. The page-granular index used in kernel pagemap interfaces (`/proc/self/pagemap`, `/proc/kpageflags`). Method: `PhysAddr::pfn()`.

A PFN identifies a 4 KiB page, not a byte. It is not interchangeable with a physical address.

**Avoid:** using PFN where a byte-granular physical address is meant.
