# ferrite-uefi

A spike exploring a **bare-metal memory-testing bootloader**: a UEFI application
that renders a [ratatui](https://ratatui.rs) UI with no OS underneath, as a proof
of concept for running ferrite's Linux RAM test
suite before (or instead of) booting an operating system.

## Status

- [x] Boots under OVMF in QEMU and renders a ratatui UI
- [x] Text-console backend with **run-coalesced** drawing (see below)
- [x] Wired networking: SNP to smoltcp, DHCP, mDNS (`ferrite.local`), HTTP status page
- [x] Incremental pattern sweep over a test buffer, with fault tracking, shown in the UI
- [x] Headless diagnostics log on the boot volume (`\ferrite.log`)
- [x] Recon apps: `pciscan` (PCI and Wi-Fi registers), `xhciscan` (USB controllers and DbC)
- [ ] Live debug serial over USB (xHCI DbC) from the app
- [ ] Wi-Fi via real Linux drivers on a Rust shim
- [ ] DNS-SD service enumeration (PTR/SRV/TXT) on top of the mDNS responder
- [ ] GOP framebuffer backend for a truecolor UI

## Quickstart

```bash
just build        # compile the .efi (x86_64-unknown-uefi)
just qemu         # boot it in QEMU with a graphical window (needs OVMF)
just qemu-serial  # boot headless, console mirrored to your terminal
```

Press `q` or `Esc` to exit. Requires `qemu-system-x86_64` and OVMF firmware at
`/usr/share/OVMF/OVMF_CODE_4M.fd` (override the `ovmf_code`/`ovmf_vars` vars in
the `justfile` if your distro puts them elsewhere).

### Deploying to real hardware

The application is `esp/efi/boot/bootx64.efi`, the default boot path UEFI
firmware looks for on removable media. Format a USB stick as FAT32 and:

```bash
just stage-usb /run/media/you/USBLABEL   # copies bootx64.efi to /efi/boot/
```

Boot the target machine from the USB (disable Secure Boot, or sign the image).

## Architecture

| File                  | Role                                                              |
| --------------------- | ----------------------------------------------------------------- |
| `src/main.rs`         | UEFI `#[entry]`; grabs stdin/stdout, runs the app                 |
| `src/app.rs`          | The ratatui UI + render/input/network/sweep poll loop             |
| `src/backend.rs`      | A ratatui `Backend` over the UEFI Simple Text Output Protocol     |
| `src/net.rs`          | smoltcp `Device` over SNP; DHCP client, HTTP server, mDNS wiring  |
| `src/mdns.rs`         | Minimal mDNS responder (parses queries, builds A answers)         |
| `src/sweep.rs`        | Incremental write/verify pattern sweep with fault tracking        |
| `src/diag.rs`         | Milestone log rewritten to `\ferrite.log` on the boot volume      |
| `src/bin/pciscan.rs`  | Standalone PCI enumerator; reads live registers off Intel Wi-Fi   |
| `src/bin/xhciscan.rs` | Standalone xHCI and DbC recon; saves a report file and auto-exits |

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

**Testing.** The QEMU recipes attach an `e1000` NIC via user-mode (SLIRP)
networking, which includes a DHCP server (guest gets `10.0.2.15`) and forwards
host `:8080` to guest `:80`:

```bash
just qemu            # boot; watch the network panel acquire a lease
curl http://127.0.0.1:8080/   # fetch the status page through the port-forward
```

SLIRP does **not** carry multicast between host and guest, so full mDNS
*discovery* can't be exercised locally. That needs bridged networking (a wired
box on the LAN, or the target hardware).

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
