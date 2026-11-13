# Code conventions

## Workspace

| Crate | Mode | Contents |
| --- | --- | --- |
| `ferrite-hal` | `no_std` | Platform traits (MMIO, PCI config, DMA, clock, delay) and the address, unit and index types every driver shares |
| `ferrite-xhci` | `no_std` + `alloc` | xHCI Debug Capability driver, written only against `ferrite-hal` |
| `ferrite-efi` | `no_std` + `uefi` | UEFI backends for the HAL traits, the boot-volume log and file reads, the `log` sink, unattended exit, the firmware watchdog, firmware RNG and RTC, the link protocol the loader shares with its apps |
| `ferrite-testing` | std | Host-only test doubles: mock HAL, simulated DbC register file |
| `ferrite-uefi` | `no_std`, UEFI app | Memory sweep, ratatui UI, SNP + smoltcp networking, mDNS, HTTP status page |
| `ferrite-recon` | `no_std`, UEFI apps | `pciscan`, `xhciscan`, `dbcping`, `dbcbench` |
| `ferrite-loader` | `no_std`, UEFI app | Resident loader: keeps the DbC up, receives pushed `.efi` images and runs them under a watchdog |
| `ferrite-wire` | `no_std` + `alloc` | Message formats spoken over the DbC link (the benchmark frames and the loader's `link` protocol), shared by both ends |
| `ferrite-lkpi` | `no_std` + `alloc` | Linux kernel API shim the Wi-Fi drivers link against; its `stubs` feature holds the generated trace-and-stop stubs |
| `ferrite-wifi-sys` | `no_std` | Pinned Linux mac80211, cfg80211, rtw89 and iwlwifi C built by `build.rs`, and the tables of its initcalls |
| `ferrite-nl80211` | `no_std` + `alloc` | Generic netlink and nl80211 messages, and a scan client over any netlink socket |
| `ferrite-wifi-start` | `no_std` | The Wi-Fi C's kernel entry points and the init thread (seam checks, initcalls, exitcalls) that the lab and `wifitrace` share |
| `ferrite-wifi-efi` | `no_std`, UEFI app built as ELF | The shim's platform pieces from firmware services (arena, guarded task stacks, firmware files, clock, PCI I/O devices, the DMA protection probe), and `wifitrace`, which runs the Wi-Fi C on them |
| `ferrite-lab` | std | The Wi-Fi C run as a Linux process on the desktop: logger, halt hook with a backtrace, the data fence, the host platform pieces |
| `ferrite-dev` | std | Desktop tool: `log` (DbC capture), `bench` (dbcbench peer), `run` (one-shot cycle on the laptop), `push` and `loader` (client of the resident loader), `build` (an app's `.efi`, converting ELF-built apps), `vendor` (Linux snapshot), `stubs` (generates the kernel-symbol stubs) |

Planned, created when their milestone starts:

| Crate | Mode | Contents |
| --- | --- | --- |
| `ferrite-wifi` | `no_std` + `alloc` | WPA2-PSK and the station state machine |
| `ferrite-hal-vfio` | std | HAL backend over Linux VFIO |

### The `no_std` line

A crate is `no_std` exactly when it links into a UEFI binary. Shared crates use
plain `#![no_std]` (with `#[cfg(test)] extern crate std` for host tests) and have
no `std` feature: Cargo unifies features across every package in one
invocation, so a host crate turning on `std` would silently reach the UEFI
build. Std-only helpers for a shared crate go in a separate std crate instead.

### Building

There is no default target. UEFI builds go through aliases in
`.cargo/config.toml` (`cargo efi-check`, `cargo efi-clippy`, `cargo efi-build`);
plain `cargo check` and `cargo nextest run` cover the host-buildable crates.
`just check`, `just lint` and `just test` run both sides.

An app that links the Linux C, which is ELF, is built for
`x86_64-unknown-none` instead (`cargo efi-elf-check`, `cargo efi-elf-clippy`)
and says so in its manifest with `[package.metadata.ferrite] target =
"x86_64-unknown-none"`. `ferrite-dev build <app>` builds either kind and
converts the ELF into the `.efi`, leaving the ELF beside it for resolving
addresses; `just stage`, `push` and `run` go through it. That target is soft
float, so such an app has no SSE.

Dev-profile optimisation is set per package, each with a stated reason; there is
no blanket `[profile.dev.package."*"]`. Generic and inline code is compiled at
the calling crate's level, so an override only helps a crate with substantial
non-generic code.

## Types

Types carry the invariants, so code past a constructor never re-checks them.

- **Addresses.** `PhysAddr` (CPU physical, MMIO), `BusAddr` (what a device DMAs
  to) and `BusAddr32` (a device address proven below 4 GiB) are distinct types
  with private fields. 64-bit pointer registers are written only from
  `BusAddr32`, which is what makes writing them as two 32-bit halves safe.
  Address arithmetic is checked.
- **Units.** Time is `core::time::Duration` at every API, and points in time are
  an `Instant`; no `_ms: u64` parameters. Sizes that are not bytes have a type
  (`PageCount`).
- **Registers.** Each register is a `Reg<T>` constant that pairs its offset with
  a typed value (bitbybit) and an access kind. Reads return the typed value.
  Write-1-to-clear bits are cleared only through an explicit acknowledge, so
  writing back a read value cannot wipe status.
- **Ranges set by hardware or a wire format** get a newtype whose constructor
  validates them: `PortNumber` (1-based, inside the controller's port range),
  DNS labels (1 to 63 bytes).
- **Codes reported by hardware or the network** (TRB types, completion codes,
  link states, record types) are enums with an `Unknown(raw)` catch-all. An
  unexpected value is logged, never a panic.
- **Plain data** with no invariant (`PciAddress`, `MacAddr`) has public fields
  and a `Display`.

## Errors

snafu, `default-features = false` with `rust_1_81`, in every crate. One enum per
module boundary; variants carry the typed context (which device, which register,
which step) and the underlying source. Results are not discarded: a failure that
is safe to ignore is logged.

## Logging

The `log` facade everywhere. Apps install `ferrite_efi::logger`, which writes
timestamped records into the boot-volume log (`\EFI\ferrite\<app>.txt`) and,
for console apps, to the screen. `error` needs a human, `warn` means degraded,
routine detail is `debug`. Lines carry values, not progress narration.

## Tests

Host-run with `cargo nextest`, using assert2 and rstest. Tests cover real logic:
ring and state-machine behaviour, wire formats against real bytes, and bugs that
happened. Hardware behaviour is checked on real hardware.

## Lints

`[workspace.lints]` with clippy `pedantic` and `nursery`, plus `unwrap_used` and
`expect_used` outside tests. Exceptions are `#[expect(lint, reason = "...")]`,
never `#[allow]`.

`clippy.toml` bans operations that must have exactly one home. The home carries
an `#[expect(clippy::disallowed_*)]`; everything else goes through it.

| Banned | Home |
| --- | --- |
| `read_volatile` / `write_volatile` | the DMA slice type, the sweep's word accesses, the ExitBootServices hook that clears DCE (no protocol calls are allowed there) |
| `_rdtsc` | `ferrite_efi::clock` |
| `uefi::boot::stall` | the UEFI `Delay` impl; loops wait on events |
| `uefi::runtime::reset` | `ferrite_efi::session` |
| `uefi::boot::set_watchdog_timer` | `ferrite_efi::watchdog` |
| `uefi::boot::exit_boot_services` | none |
| `FileSystem::write` (truncating rewrite) | none; files go through `BootLog` |
| `uefi::println` / `print` | `BootLog`'s console echo and the panic handler |
| `assert_eq` / `assert_ne` | none; assert2's `check!` / `assert!` |
| std `Mutex`, `RwLock`, `HashMap`, `HashSet` | none; parking_lot and rustc_hash in the std crates |
