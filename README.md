# ferrite

[![CI](https://github.com/Xevion/ferrite/actions/workflows/check.yml/badge.svg)](https://github.com/Xevion/ferrite/actions/workflows/check.yml)
[![Coverage](https://codecov.io/gh/Xevion/ferrite/branch/master/graph/badge.svg)](https://codecov.io/gh/Xevion/ferrite)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue.svg)](#license)

A bare-metal RAM tester for x86 PCs, written in Rust, and a modern alternative to memtest86+.

Named after ferrite core memory: the dominant RAM technology of the 1960s and 70s, where tiny iron rings were magnetized to store individual bits. Testing RAM with iron.

## The UEFI tester

ferrite boots as a UEFI application with no operating system underneath. It runs a 14-test suite (memtest86+'s tests plus March C- and others) on SIMD kernels from scalar to AVX-512, split across every core, and draws a dashboard on the framebuffer. It also serves a live status page and a JSON API with run controls over wired and Wi-Fi networking, so a machine with no screen or keyboard can be tested from a USB stick and watched from another device. The scope is that of memtest86+: test nearly all of a PC's RAM and report on it.

Status:

- [x] Boots under OVMF in QEMU and on real hardware
- [x] Framebuffer dashboard, with a text-console fallback where there is no GOP
- [x] Wired networking through the firmware's SNP driver and Wi-Fi at once: DHCP, mDNS (`ferrite.local`), a live status page, a JSON API and run controls. Verified on real hardware over Wi-Fi (Intel 8265) and under QEMU over Ethernet; a real NIC's SNP driver has not been tried yet
- [x] Wi-Fi through real Linux drivers on a Rust shim, with a Rust WPA2/WPA3 supplicant. The Intel 8265 is the card run on hardware; other Intel and MediaTek cards are compiled but not yet run
- [x] Push notifications over plain HTTP (e.g. ntfy) at the end of a pass, on the first fault and at the end of the run
- [x] Headless diagnostics log on the boot volume
- [x] USB debug serial (xHCI DbC) and a resident loader for a fast remote test loop
- [ ] DNS-SD service enumeration on top of the mDNS responder
- [ ] Leaving UEFI boot services entirely (a later stage in the roadmap)

Secure Boot has to be off, since the images are unsigned.

### Build and boot

```bash
just qemu                         # boot in QEMU with a window (needs qemu-system-x86_64 and OVMF)
just qemu ferrite-uefi serial     # boot headless, console mirrored to the terminal
just stage-usb /run/media/you/USBLABEL   # stage onto a FAT32 USB stick, then boot a PC from it
```

In the app, `p` pauses or resumes, `r` restarts, and `q` or `Esc` leaves. Settings (Wi-Fi networks, notifications, time zone, CPU policy) live in `\EFI\ferrite\ferrite.toml` on the boot volume. The UEFI app has its own README at [crates/ferrite-uefi/README.md](crates/ferrite-uefi/README.md).

## Workspace

| Crate | Role |
| --- | --- |
| `ferrite-uefi` | The UEFI app: memory sweep on every core, dashboard, status page and API |
| `ferrite-mem` | The memory tests: kernels per SIMD tier, the test suite, fault analysis, the ETA model |
| `ferrite-hal`, `ferrite-rt` | Platform traits and shared types for drivers; libc symbols for the ELF-built apps |
| `ferrite-efi` | UEFI backends for the HAL traits, the boot-volume log, CPU clock control |
| `ferrite-power` | Clock, energy and limit registers on Intel and AMD, and the clock governor |
| `ferrite-net`, `ferrite-config` | Network stack (DHCP, mDNS, SNTP, HTTP) over any port; the `ferrite.toml` settings file |
| `ferrite-wifi`, `ferrite-wifi-efi`, `ferrite-wifi-start`, `ferrite-wpa`, `ferrite-nl80211` | The Wi-Fi station: supervisor, UEFI runtime, WPA2/WPA3 supplicant, nl80211 client |
| `ferrite-lkpi`, `ferrite-wifi-sys`, `ferrite-lab` | Linux kernel API shim, the pinned Linux Wi-Fi C, and a desktop harness for running it |
| `ferrite-xhci`, `ferrite-wire`, `ferrite-loader`, `ferrite-recon` | USB debug serial driver, its wire formats, the resident loader, recon apps |
| `ferrite-dev`, `ferrite-testing` | Desktop tool (build, push, logs, settings) and host-only test doubles |
| `ferrite-linux` | The Linux userspace tester (package `ferrite`), below |

Crate boundaries, the `no_std` line and type, error and logging rules are in [docs/CONVENTIONS.md](docs/CONVENTIONS.md).

## The Linux tool

`ferrite-linux` (package and binary `ferrite`) is the original userspace memory tester. It allocates a region of RAM, locks it against swapping with `mlock`, and runs test patterns to detect stuck bits, address aliasing and coupling faults while the OS is live. It is a fast sanity check you can run without a reboot, not a replacement for the UEFI tester or memtest86+.

Patterns: Solid Bits, Walking Ones/Zeros, Checkerboard, Stuck Address, and March C-. Beyond fill-and-verify, it resolves physical addresses, reads ECC/EDAC counters and DIMM topology so failures can be pinned to a module, classifies bit errors as stuck-bit or coupling, tracks which physical frames have been tested across runs and reboots, and can test a physical range directly through `/dev/mem`. Hot paths use AVX-512 non-temporal stores where available and run in parallel via Rayon. There is an inline TUI for live progress and NDJSON output for scripting.

```bash
cargo build --release -p ferrite   # recent stable Rust, 2024 edition

ferrite                            # test 64M of RAM (default)
ferrite --size max                 # as much as available, minus headroom
ferrite --passes 3                 # three passes over the allocation
ferrite --test solid-bits --test checkerboard
ferrite --format json              # NDJSON event stream to stdout
ferrite --coverage-file coverage.json --cull   # cumulative physical coverage across runs
```

Large allocations need enough `RLIMIT_MEMLOCK`: run as root, raise it with `ulimit -l unlimited`, or grant `CAP_IPC_LOCK` to the binary. Userspace testing cannot reach all of RAM because the kernel, other processes and ferrite itself occupy memory it cannot touch. `--cull` and cross-run coverage tracking raise the ceiling, but for full coverage use the UEFI tester.

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.

The exception is `crates/ferrite-wifi-sys`, which contains code derived from the Linux kernel and is GPL-2.0-only (see its `LICENSE`). Any binary that links it is distributed under GPL-2.0, and that includes the UEFI app, which links it through `ferrite-wifi-efi`.
