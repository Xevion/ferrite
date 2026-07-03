# ferrite

[![CI](https://github.com/Xevion/ferrite/actions/workflows/check.yml/badge.svg)](https://github.com/Xevion/ferrite/actions/workflows/check.yml)
[![Coverage](https://codecov.io/gh/Xevion/ferrite/branch/master/graph/badge.svg)](https://codecov.io/gh/Xevion/ferrite)
[![License: GPL-3.0](https://img.shields.io/badge/License-GPL--3.0-blue.svg)](LICENSE)

A userspace memory tester for Linux, written in Rust.

Named after ferrite core memory -- the dominant RAM technology of the 1960s and 70s, where tiny iron rings were magnetized to store individual bits. Testing RAM with iron.

## What it does

Allocates a region of physical RAM, locks it against swapping via `mlock`, then runs a suite of test patterns to detect stuck bits, address aliasing, and coupling faults. Designed to run while the OS is live -- not a replacement for memtest86+, but a fast sanity check you can run without a reboot.

Patterns: Solid Bits, Walking Ones/Zeros, Checkerboard, Stuck Address, and March C- (a sequential march test that also catches coupling and address-decoder faults the others miss).

Beyond fill-and-verify, ferrite resolves physical addresses, reads ECC/EDAC counters and DIMM topology so failures can be pinned to a real module, classifies bit errors as stuck-bit vs coupling, tracks which physical frames have been tested across runs and reboots, and can test a specific physical range directly through `/dev/mem`. All hot paths use AVX-512 non-temporal stores where available and run in parallel via Rayon. There's an inline TUI for live progress and NDJSON output for scripting.

## Quick start

```bash
# Test 64M of RAM (default)
ferrite

# Test as much as available, minus headroom for the rest of the system
ferrite --size max

# Run 3 passes over the allocation
ferrite --passes 3

# Run only specific patterns
ferrite --test solid-bits --test checkerboard

# NDJSON event stream to stdout
ferrite --format json

# Track cumulative physical coverage across runs
ferrite --coverage-file coverage.json --cull
```

Large allocations require sufficient `RLIMIT_MEMLOCK` -- either run as root, raise the limit with `ulimit -l unlimited`, or grant `CAP_IPC_LOCK` to the binary.

## Limitations

Userspace testing cannot reach 100% of RAM -- the kernel, running processes, and ferrite itself occupy memory that can't be touched. `--cull` and cross-run coverage tracking push the reachable ceiling higher, but for comprehensive coverage use memtest86+.

## Building

```bash
cargo build --release
```

Requires a recent stable Rust toolchain (2024 edition).

## License

[GPL-3.0](LICENSE)
