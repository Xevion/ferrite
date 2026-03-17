# ferrite

A userspace memory tester for Linux, written in Rust.

Named after ferrite core memory — the dominant RAM technology of the 1960s and 70s, where tiny iron rings were magnetized to store individual bits. Testing RAM with iron.

## What it does

Allocates as much physical RAM as possible, locks it against swapping, then hammers it with a suite of test patterns to detect stuck bits, address line faults, coupling errors, and other common failure modes. Designed to be run while the OS is live — not a replacement for memtest86+, but a fast, ergonomic sanity check you can run without a reboot.

## Quick Start

```bash
# Test as much RAM as possible (requires root for mlock)
sudo ferrite

# Test a specific amount
sudo ferrite --size 8G

# Run a specific number of loops
sudo ferrite --loops 3

# JSON output for scripting
sudo ferrite --json
```

## Why not memtester?

memtester works. ferrite aims to be:
- More informative output (which bits flipped, error density, suspected region)
- Physical address resolution (when run as root on a cooperative kernel)
- NUMA-aware (test per-node, or test all nodes)
- ECC-aware (read correctable error counts before/after)
- Faster iteration via parallelism

## Limitations

Userspace testing cannot reach 100% of RAM — the kernel, running processes, and ferrite itself occupy memory that can't be touched. Expect to test roughly 70-85% of installed RAM. For comprehensive coverage, use memtest86+.
