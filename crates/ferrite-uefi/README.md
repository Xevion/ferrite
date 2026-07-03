# ferrite-uefi

A spike exploring a **bare-metal memory-testing bootloader**: a UEFI application
that renders a [ratatui](https://ratatui.rs) UI with no OS underneath, as a proof
of concept for running ferrite's Linux RAM test
suite before (or instead of) booting an operating system.

It does not test RAM
yet; the current milestone is a live render loop on real UEFI firmware.

## Status

- [x] Boots under OVMF in QEMU and renders an animated ratatui UI
- [x] Text-console backend with **run-coalesced** drawing (see below)
- ⬜ Wired networking (mDNS advertisement + tiny HTTP status page)
- ⬜ A real memory sweep wired into the UI
- ⬜ GOP framebuffer backend for a truecolor UI
- [ ] WiFi: not feasible from a bare-metal UEFI app (see [WiFi](#wifi))

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

The application is `esp/efi/boot/bootx64.efi`, the default boot path a UEFI
firmware looks for on removable media. Format a USB stick as FAT32 and:

```bash
just stage-usb /run/media/you/USBLABEL   # copies bootx64.efi to /efi/boot/
```

Boot the target machine from the USB (disable Secure Boot, or sign the image).

## Architecture

| File            | Role                                                              |
| --------------- | ---------------------------------------------------------------- |
| `src/main.rs`   | UEFI `#[entry]`; grabs stdin/stdout, runs the app                 |
| `src/app.rs`    | The ratatui UI + render/input loop                               |
| `src/backend.rs`| A ratatui `Backend` over the UEFI Simple Text Output Protocol    |

### The backend, and the performance note

The backend is vendored from [`ratatuefi`](https://github.com/sermuns/ratatuefi)
(MIT/Apache) and reworked. It renders to the **text console** (Simple Text Output
Protocol): a 16-colour character grid, not a pixel framebuffer.

Upstream issued three firmware protocol calls *per character cell*
(`set_cursor_position` + `set_color` + `write_str`), which is slow on real
firmware where each call is expensive. This version **coalesces contiguous,
identically-styled cells on a row into a single write**, and only re-issues
cursor/color calls when they change. Combined with ratatui's own frame diffing
(only changed cells reach the backend), that collapses "calls per character" into
"calls per styled run."

A **GOP framebuffer backend** with an embedded bitmap font is the path to a
truecolor UI; the text console is the fast route to something on screen.

## WiFi

The original dream was a headless tester that hosts a web server over the
laptop's WiFi and advertises itself via **mDNS/DNS-SD** (Bonjour / Zeroconf, what
Philips Hue and macOS AirPrint use). Networking *is* reachable over **wired
ethernet**: OVMF exposes the full UEFI network stack (SNP -> MNP -> UDP4/TCP4/DHCP4),
so DHCP + a hand-rolled mDNS responder (just UDP multicast to `224.0.0.251:5353`)
+ a tiny HTTP server is a genuinely achievable milestone in QEMU and on a wired
box on the LAN.

**WiFi, however, is effectively a dead end from a bare-metal UEFI app**, and not
for lack of effort. It's a missing-driver problem:

- There is no OS/kernel here, so there is no borrowed WiFi stack.
- UEFI *does* specify a WiFi stack (`EFI_WIRELESS_MAC_CONNECTION_II_PROTOCOL` +
  supplicant/EAP, driven by EDK2's `WifiConnectionManagerDxe`), but that stack
  only *consumes* a chipset driver that must already publish the protocol on the
  NIC handle. That producer driver is **proprietary, per-chipset, and welded into
  specific OEM firmware builds** (HP Sure Recover / Dell SupportAssist-class, Intel
  AX2xx/AC-9xxx cards only), so it is not a redistributable `.efi` you can load from
  a USB stick, and non-Intel consumer chipsets have no UEFI wireless driver at all.
- USB WiFi dongles don't ship UEFI wireless drivers either; iPXE's ancient 802.11
  support was WEP-only and is dead.

So "flash a USB, write SSID/credentials, connect on a variety of laptops" is not a
weeks-vs-months scoping question; no amount of our own engineering closes it,
because the per-chipset driver isn't available to us. The realistic story is
**wired networking**; WiFi stays a research note. (Full findings are worth
capturing in an issue if we revisit.)

## License

MIT OR Apache-2.0, matching the vendored `ratatuefi` backend.
