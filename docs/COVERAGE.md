# Physical Coverage Architecture

How ferrite knows *which* physical memory it has tested, across runs and reboots,
and how it reaches memory it hasn't.

## The three questions

1. **What did this run test?** — answered per-run by resolving every locked page
   to its physical frame (PFN) via `/proc/self/pagemap`. Implemented (XEV-1016):
   `MapStats.resolved_pages` x 4 KiB vs the `/proc/iomem` "System RAM" denominator.
   Gap: the PFN *set* is discarded after counting — only a byte total survives.
2. **What has ever been tested?** — requires persisting the PFN set and merging
   across runs (XEV-536). Physical frame numbers are stable across reboots (the
   hardware doesn't move), so a cumulative map stays valid for the life of the
   machine's memory configuration.
3. **How do we test what hasn't been?** — userspace cannot ask the kernel for
   specific physical frames. We can only influence acquisition statistically
   (allocation pressure, reboots reshuffling occupancy) or structurally
   (frame-hostage culling, below). The last few percent (kernel text, reserved
   ranges, pinned slab) are unreachable from userspace by design; the honest
   ceiling is ~80-90% on a quiet machine. Full coverage belongs to the
   kexec/bootable strategy (RESEARCH.md section 14, XEV-547/549).

## Unit of tracking

The 4 KiB page frame (PFN). Everything above (THP, chunks, allocations) reduces
to sets of PFNs. A full bitmap of a 32 GiB machine is 1 MiB (1 bit per frame) —
in-memory representation is a bitmap; on-disk representation is compacted RLE
ranges (THP backing makes runs long, so ranges stay small).

## Coverage store (XEV-536)

A versioned JSON file, explicit opt-in via `--coverage-file <path>`:

- **Fingerprint guard**: the map is only meaningful for a fixed physical layout.
  Store `MemTotal` + a hash of the `/proc/iomem` System RAM ranges (+ DMI product
  UUID when readable). On mismatch, refuse and require a new file (memory was
  added/removed/re-fenced; old coverage is meaningless).
- **Cumulative PFN ranges**: the union of all successfully tested frames.
  A frame counts as covered only if every selected pattern completed against it
  in some run (partial/cancelled passes don't merge).
- **Run log**: per run — timestamp, boot_id, patterns, passes, bytes tested,
  new bytes contributed, failures found.
- On Unraid the rootfs is ephemeral: point `--coverage-file` at persistent
  storage (e.g. `/mnt/user/appdata/ferrite/coverage.json`).

Reporting:

- Startup: `Cumulative coverage: 24.1 / 31.9 GiB (75.6%) across 3 runs`
- Completion: `This run tested 26.0 GiB (3.2 GiB new) -> cumulative 27.3 GiB (85.7%)`
- NDJSON `run_complete.coverage` gains `new_bytes`, `cumulative_bytes`, `runs`.

## Reaching more RAM

### Tier 1 — don't die (XEV-535)

A single giant mmap + parallel fault OOM-kills the process when the request
exceeds what the kernel can reclaim (observed: 28G request on a 32G box, exit
137, zero results). Fix: reserve the full request as `PROT_NONE`, then activate
in chunks — `mprotect(RW)` + parallel fault + `mlock` per chunk, checking
`MemAvailable` headroom between chunks. First failure (mlock error or headroom
floor) trims the tail: the run proceeds with what was achieved, reported
honestly (`requested 28 GiB, locked 24.5 GiB`). The virtual range stays
contiguous, so the runner/pattern layer is untouched. `-s max` = MemAvailable
minus headroom.

### Tier 2 — shuffle (free)

Consecutive runs mostly re-acquire the same frames (buddy allocator LIFO), but
service restarts and reboots redistribute occupancy. With the coverage store,
each reboot-and-run visibly grows the cumulative number; the delta report tells
the user whether another cycle is still paying off.

### Tier 3 — frame-hostage culling (planned)

To *actively* acquire untested frames within one boot: allocate chunks
unlocked, resolve PFNs, and keep already-covered frames mapped as hostages
(resident anon pages can't be handed out again; no swap needed) while
continuing to allocate — the kernel is forced to serve fresh frames. When the
budget is reached, release the hostages (munmap), lock the survivors, re-verify
PFN stability, and test only new frames. Culling granularity is 2 MiB (THP
block) to avoid splitting huge pages. This inverts the current pipeline
(fault -> resolve -> cull -> mlock instead of fault -> mlock -> resolve);
`madvise(MADV_DONTNEED)` fails with EINVAL on locked VMAs, so culling must
precede mlock.

### Tier 4 — leave userspace (research track)

kexec phase-swapped `memmap=` reservations, `/dev/mem` direct testing, memory
hotplug offline/online cycles: XEV-549, XEV-538, XEV-539. Out of scope here.

## Honest denominators (planned)

"Coverage: 81%" hides whether the remaining 19% is reachable. A system-wide
`/proc/kpageflags` scan classifies every frame: free/buddy (acquirable), page
cache (reclaimable), anon in use (freed by stopping services), slab/kernel/
reserved (unreachable from userspace). The gap report should say: `untested:
6.1 GiB = 2.3 GiB acquirable + 1.9 GiB reclaimable + 1.9 GiB unreachable`, so
"90% and everything else is pinned" reads as *done* rather than *incomplete*.

## Staging

| Stage | Issue | Status |
|---|---|---|
| Single-run coverage % | XEV-1016 | Done |
| Chunked OOM-safe allocation, `-s max` | XEV-535 | Implemented |
| PFN-range export + coverage store + delta reporting | XEV-536 | Implemented (v1) |
| Frame-hostage culling (`--cull`) | XEV-1018 | Implemented |
| kpageflags gap classification | XEV-1019 | Implemented |
| Fault-class-aware coverage | XEV-561 (needs XEV-559) | Backlog |
| kexec / bootable full coverage | XEV-547/549 | Research |
