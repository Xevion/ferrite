# ferrite-uefi

A spike exploring a **bare-metal memory-testing bootloader**: a UEFI application
that tests RAM on every core, draws a dashboard on the framebuffer and serves a
live status page, with no OS underneath, as a proof of concept for running ferrite's Linux RAM test
suite before (or instead of) booting an operating system.

## Status

- [x] Boots under OVMF in QEMU and on real hardware
- [x] Framebuffer dashboard drawn with tiny-skia and fontdue, written write-combining; a short text screen where there is no GOP (see Screen)
- [x] Wired networking: SNP to smoltcp, DHCP, mDNS (`ferrite.local`), a live status page with a JSON API and run controls. Verified under QEMU only, not yet on real hardware (see Networking)
- [x] A 14-test suite (memtest86+'s tests plus March C- and others) on SIMD kernels from scalar to AVX-512, split over every core through MP Services
- [x] Headless diagnostics log on the boot volume (`\EFI\ferrite\ferrite-uefi.txt`)
- [x] Recon apps: `pciscan` (PCI and Wi-Fi registers), `xhciscan` (USB controllers and DbC), `dbcping` and `dbcbench` (DbC link)
- [x] USB debug serial (xHCI DbC) driver, verified on real hardware with `dbcping` and `dbcbench` (326 MB/s target to desktop, 189 MB/s back)
- [x] Resident loader: the desktop pushes `.efi` images over DbC and gets live logs and exit status back, with no reboot between runs
- [x] DbC log output from the main app, through the loader
- [ ] Wi-Fi via real Linux drivers on a Rust shim
- [ ] DNS-SD service enumeration (PTR/SRV/TXT) on top of the mDNS responder

## Quickstart

```bash
just build                        # compile every UEFI app
just qemu                         # boot the main app in QEMU with a graphical window (needs OVMF)
just qemu ferrite-uefi serial     # boot headless, console mirrored to your terminal
just qemu xhciscan serial         # any app by name: ferrite-uefi, pciscan, xhciscan, dbcping, dbcbench
```

`just qemu <app> <gui|serial> <user|tap>` stages the app as `esp/efi/boot/bootx64.efi`
and boots it with an `e1000` NIC and a `qemu-xhci` controller. In the main app `p`
pauses or resumes, `r` restarts, and `q` or `Esc` leaves. Requires `qemu-system-x86_64` and OVMF firmware at
`/usr/share/OVMF/OVMF_CODE_4M.fd` (override the `ovmf_code` and `ovmf_vars`
variables in the `Justfile` if your distro puts them elsewhere). QEMU has no
xHCI Debug Capability, so the DbC apps only run to "no DbC found" there.

Every app writes its log to `\EFI\ferrite\<app>.txt` on the boot volume (for
example `\EFI\ferrite\xhciscan.txt`). `just qemu` boots from a FAT image built
from `esp/` (QEMU's `fat:rw:` folder drive corrupts growing files and can abort
QEMU), and `just qemu-log <app>` prints that log after a run.

### Deploying to real hardware

The staged application is `esp/efi/boot/bootx64.efi`, the default boot path UEFI
firmware looks for on removable media. Format a USB stick as FAT32 and:

```bash
just stage-usb /run/media/you/USBLABEL            # main app
just stage-usb /run/media/you/USBLABEL dbcping    # another app
```

Boot the target machine from the USB (disable Secure Boot, or sign the image).

## Architecture

| Crate | Role |
| --- | --- |
| `ferrite-hal` | Platform traits (MMIO, PCI config, DMA, clock, delay) and the address, time and register types drivers share |
| `ferrite-xhci` | xHCI Debug Capability driver, written only against `ferrite-hal` |
| `ferrite-efi` | UEFI backends for those traits, the boot-volume log, the `log` sink, unattended exit |
| `ferrite-testing` | Host-only test doubles: mock HAL, simulated DbC register file |
| `ferrite-mem` | The memory tests: kernels per SIMD tier, the test suite, fault analysis |
| `ferrite-uefi` | The main app: memory sweep on every core (`sweep.rs`, `crew.rs`), run history and commands (`report.rs`, `control.rs`), framebuffer dashboard (`dash/`, `gfx/`, `gop.rs`) with a text fallback (`ui.rs`, `backend.rs`), SNP + smoltcp networking and the status page (`net/`), mDNS (`mdns.rs`), headless log (`diag.rs`) |
| `ferrite-recon` | The standalone apps `pciscan`, `xhciscan`, `dbcping`, `dbcbench`, and the code they share |

The crate boundaries, the `no_std` line and the type, error and logging rules are
in [docs/CONVENTIONS.md](docs/CONVENTIONS.md).

### Networking

There are no UDP/TCP wrappers in `uefi` 0.38 (only `http` *client*, `ip4config2`,
`pxe`, `snp`), so hosting anything means driving raw L2 frames. `net.rs`
implements a smoltcp [`Device`](https://docs.rs/smoltcp/latest/smoltcp/phy/trait.Device.html)
over the Simple Network Protocol (`transmit`/`receive`/`mode`), and smoltcp
supplies the rest of the stack:

- **DHCPv4**: acquires an address, shown on the dashboard once leased.
- **HTTP** (TCP :80): picoserve over smoltcp sockets, polled from the main loop.
  `/` is a self-contained live dashboard; `/api/status` is the run as JSON;
  `/api/events` streams it as server-sent events; `POST /api/{pause,resume,restart,cores,quit}`
  control the run. Controls need `Content-Type: application/json`, which a
  cross-site form cannot send.
- **mDNS** (UDP :5353): answers `A` queries for `ferrite.local` so a laptop on
  the same segment can browse to `http://ferrite.local`.

`wait_for_packet_event` is documented as never firing under QEMU, so the stack
*polls* `receive` on every render tick rather than blocking on the event.

**Testing.** `just qemu` attaches the `e1000` NIC through user-mode (SLIRP)
networking, which includes a DHCP server (guest gets `10.0.2.15`) and forwards
host `:8080` to guest `:80`:

```bash
just qemu            # boot; watch the network panel acquire a lease
curl http://127.0.0.1:8080/   # fetch the status page through the port-forward
```

SLIRP does not carry multicast between host and guest, so mDNS needs the `tap`
mode, which joins a tap device to libvirt's NAT bridge `virbr0`. Create the tap
once per boot of the host (the two commands vanish on reboot):

```bash
sudo ip tuntap add dev tap-ferrite mode tap user $USER
sudo ip link set tap-ferrite master virbr0 up
just qemu ferrite-uefi gui tap
```

`virbr0` is libvirt's private NAT network and is never bridged to the physical
NIC, so the guest cannot reach or disturb the LAN. Its DHCP server hands the
guest a lease, and the host reaches the guest at `ferrite.local` over the same
bridge.

Verified under QEMU/OVMF with an `e1000` on `tap-ferrite` on `virbr0`, on QEMU's
iPXE SNP driver:

- DHCP lease acquired.
- `ferrite.local` resolves through avahi and through the system resolver.
- The guest answers ping.
- HTTP gives 200 for `/` (59 ms), 404 for `/favicon.ico` and 405 for `POST /`.

This is the first time mDNS has been verified; before it only DHCP and HTTP
through SLIRP were. Nothing here has run on real hardware or on a real NIC's SNP
driver yet. Routing, the control bodies and end-to-end requests over a smoltcp
loopback are covered by unit tests in `crates/ferrite-uefi/src/net/http/`.

### Screen

With a GOP the app switches to the display's native mode (the EDID's preferred
timing) and draws a dashboard: tiny-skia for shapes, fontdue for text from
embedded Inter and JetBrains Mono, into a BGRA canvas the size of the screen.
Each widget redraws only when its inputs change, and only the pixels that differ
are pushed.

Firmware usually leaves the framebuffer aperture uncacheable, and `memcpy` here is
`rep movsb`, which that turns into one bus write per byte (5 MB/s on the laptop). Rows
go out in 16-byte stores instead, with the framebuffer remapped write-combining
for the app's lifetime through a spare PAT entry
(`ferrite_efi::paging::WriteCombining`): 4.6 GB/s on the laptop, against 96 MB/s for
the firmware's `Blt`. Startup times each path on a strip and keeps the fastest.

Without a GOP the app falls back to a few lines of ratatui on the text console,
through a backend derived from [`ratatuefi`](https://github.com/sermuns/ratatuefi)
(MIT/Apache) that writes runs of same-styled cells in one call.

## Wi-Fi

The goal is a headless tester that joins the machine's own network, wired or
Wi-Fi, serves its status page, and advertises itself over mDNS.

Wired networking works today through the firmware's SNP driver. Wi-Fi has no
ready-made UEFI driver for most chips: firmware Wi-Fi exists only on some
business laptops and only for recent Intel cards. So ferrite supplies its own.
The plan is to compile Linux's real Wi-Fi drivers and its `mac80211`/`cfg80211`
stack unchanged, link them into the app, and implement the Linux kernel
functions they call in a Rust shim. That is the approach Genode uses to run
unmodified Linux Wi-Fi drivers. A handful of chip families covers most PCs, so
one shim plus their Linux drivers covers most machines.

## License

MIT OR Apache-2.0, matching the vendored `ratatuefi` backend.
