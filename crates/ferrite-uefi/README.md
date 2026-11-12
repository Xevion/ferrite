# ferrite-uefi

A spike exploring a **bare-metal memory-testing bootloader**: a UEFI application
that renders a [ratatui](https://ratatui.rs) UI with no OS underneath, as a proof
of concept for running ferrite's Linux RAM test
suite before (or instead of) booting an operating system.

## Status

- [x] Boots under OVMF in QEMU and renders a ratatui UI
- [x] Text-console backend with **run-coalesced** drawing (see below)
- [x] Wired networking: SNP to smoltcp, DHCP, mDNS (`ferrite.local`), HTTP status page. Verified under QEMU only, not yet on real hardware (see Networking)
- [x] Incremental pattern sweep over a test buffer, with fault tracking, shown in the UI
- [x] Headless diagnostics log on the boot volume (`\EFI\ferrite\ferrite-uefi.txt`)
- [x] Recon apps: `pciscan` (PCI and Wi-Fi registers), `xhciscan` (USB controllers and DbC), `dbcping` and `dbcbench` (DbC link)
- [x] USB debug serial (xHCI DbC) driver, verified on real hardware with `dbcping` and `dbcbench` (326 MB/s target to desktop, 189 MB/s back)
- [x] Resident loader: the desktop pushes `.efi` images over DbC and gets live logs and exit status back, with no reboot between runs
- [x] DbC log output from the main app, through the loader
- [ ] Wi-Fi via real Linux drivers on a Rust shim
- [ ] DNS-SD service enumeration (PTR/SRV/TXT) on top of the mDNS responder
- [ ] GOP framebuffer backend for a truecolor UI

## Quickstart

```bash
just build                        # compile every .efi (x86_64-unknown-uefi)
just qemu                         # boot the main app in QEMU with a graphical window (needs OVMF)
just qemu ferrite-uefi serial     # boot headless, console mirrored to your terminal
just qemu xhciscan serial         # any app by name: ferrite-uefi, pciscan, xhciscan, dbcping, dbcbench
```

`just qemu <app> <gui|serial> <user|tap>` stages the app as `esp/efi/boot/bootx64.efi`
and boots it with an `e1000` NIC and a `qemu-xhci` controller. Press `q` or `Esc`
to leave the main app. Requires `qemu-system-x86_64` and OVMF firmware at
`/usr/share/OVMF/OVMF_CODE_4M.fd` (override the `ovmf_code` and `ovmf_vars`
variables in the `Justfile` if your distro puts them elsewhere). QEMU has no
xHCI Debug Capability, so the DbC apps only run to "no DbC found" there.

Every app writes its log to `\EFI\ferrite\<app>.txt` on the boot volume (for
example `\EFI\ferrite\xhciscan.txt`). QEMU's `fat:rw:` drive corrupts files that
grow, so inspect those logs from a real FAT image instead.

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
| `ferrite-uefi` | The main app: ratatui UI (`ui.rs`, `backend.rs`), memory sweep (`sweep.rs`), SNP + smoltcp networking (`net/`), mDNS (`mdns.rs`), headless log (`diag.rs`) |
| `ferrite-recon` | The standalone apps `pciscan`, `xhciscan`, `dbcping`, `dbcbench`, and the code they share |

The crate boundaries, the `no_std` line and the type, error and logging rules are
in [docs/CONVENTIONS.md](docs/CONVENTIONS.md).

### Networking

There are no UDP/TCP wrappers in `uefi` 0.38 (only `http` *client*, `ip4config2`,
`pxe`, `snp`), so hosting anything means driving raw L2 frames. `net.rs`
implements a smoltcp [`Device`](https://docs.rs/smoltcp/latest/smoltcp/phy/trait.Device.html)
over the Simple Network Protocol (`transmit`/`receive`/`mode`), and smoltcp
supplies the rest of the stack:

- **DHCPv4**: acquires an address; shown live in the UI as it transitions from
  `requesting (DHCP)...` to the leased CIDR + gateway + DNS.
- **HTTP** (TCP :80): a single-connection HTTP/1.0 status page reflecting the
  live tester state (IP, MAC, sweep %).
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
driver yet. The routing and status codes are also covered by unit tests in
`crates/ferrite-uefi/src/net/http.rs`; the QEMU results above are from a manual
run with no recorded artifact.

### The backend, and the performance note

The backend is vendored from [`ratatuefi`](https://github.com/sermuns/ratatuefi)
(MIT/Apache) and reworked. It renders to the **text console** (Simple Text Output
Protocol), a 16-colour character grid, not a pixel framebuffer.

Upstream issued three firmware protocol calls *per character cell*
(`set_cursor_position` + `set_color` + `write_str`), which is slow on real
firmware where each call is expensive. This version **coalesces contiguous,
identically-styled cells on a row into a single write**, and only re-issues
cursor/color calls when they change. Combined with ratatui's own frame diffing
(only changed cells reach the backend), that collapses "calls per character" into
"calls per styled run."

A **GOP framebuffer backend** with an embedded bitmap font is the path to a
truecolor UI; the text console is the fast route to something on screen.

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
