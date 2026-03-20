# ferrite

[![CI](https://github.com/Xevion/ferrite/actions/workflows/check.yml/badge.svg)](https://github.com/Xevion/ferrite/actions/workflows/check.yml)
[![Coverage](https://codecov.io/gh/Xevion/ferrite/branch/master/graph/badge.svg)](https://codecov.io/gh/Xevion/ferrite)
[![MSRV](https://img.shields.io/badge/MSRV-1.85-orange?logo=rust&logoColor=white)](https://blog.rust-lang.org/2025/02/20/Rust-1.85.0.html)
[![License: GPL-3.0](https://img.shields.io/badge/License-GPL--3.0-blue.svg)](LICENSE)

A userspace memory tester for Linux, written in Rust.

Named after ferrite core memory — the dominant RAM technology of the 1960s and 70s, where tiny iron rings were magnetized to store individual bits. Testing RAM with iron.

## What it does

Allocates a region of physical RAM, locks it against swapping via `mlock`, then runs a suite of test patterns to detect stuck bits, address line faults, coupling errors, and other common failure modes. Designed to run while the OS is live — not a replacement for memtest86+, but a fast sanity check you can run without a reboot.

### Current test patterns

- **Solid Bits** — fills with all-ones then all-zeros
- **Walking Ones / Walking Zeros** — walks a single set/cleared bit across each 64-bit word
- **Checkerboard** — alternating `0x55...55` / `0xAA...AA` patterns
- **Stuck Address** — writes each word's index, verifies the address mapping is intact

All writes use non-temporal stores (AVX-512 when available) and all reads/writes use volatile operations to prevent the compiler from optimizing away memory accesses.

## Quick start

```bash
# Test 64M of RAM (default)
ferrite

# Test a specific amount
ferrite --size 4G

# Run 3 passes over the allocation
ferrite --passes 3

# Run only specific tests
ferrite --test solid-bits --test checkerboard

# NDJSON event stream to stdout
ferrite --json

# NDJSON to a file, human output to stdout
ferrite --json results.ndjson
```

Large allocations require sufficient `RLIMIT_MEMLOCK` — either run as root, raise the limit with `ulimit -l unlimited`, or grant `CAP_IPC_LOCK` to the binary.

## Why not memtester?

memtester works. ferrite aims to be:

- **More informative** — reports which bits flipped, at what offset, in what pattern
- **Parallel** — uses all available cores via Rayon for write and verify phases
- **Scriptable** — NDJSON output mode for machine consumption
- **Faster** — AVX-512 non-temporal stores where available, avoiding cache pollution

### Planned

- Physical address resolution (via `/proc/self/pagemap`)
- NUMA-aware allocation and per-node testing
- ECC monitoring (EDAC counters before/after)

## Limitations

Userspace testing cannot reach 100% of RAM — the kernel, running processes, and ferrite itself occupy memory that can't be touched. For comprehensive coverage, use memtest86+.

## Building

```bash
cargo build --release
```

Requires Rust 1.85+ (2024 edition).

## License

[GPL-3.0](LICENSE)
