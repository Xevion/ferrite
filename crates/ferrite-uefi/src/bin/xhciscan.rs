//! `xhciscan`: find the xHCI USB host controller, map its MMIO, and enumerate
//! what we can actually *do* with its ports from bare metal. This is the recon
//! for host-to-host USB communication:
//!
//!   - Does the controller support **Port Power Control** (HCCPARAMS1.PPC)? If so,
//!     the `PORTSC.PP` bit lets us toggle VBUS on a port from software, a
//!     one-bit transmit line over any cable, no device role needed.
//!   - Is the **Debug Capability (DbC)** present in the extended-capability list?
//!     That's the real bidirectional USB-serial channel (needs a crossover cable).
//!   - Per-port live state: connected? powered? link state? speed?
//!
//! Runs on real hardware and under QEMU (`-device qemu-xhci`). Register layout
//! per the xHCI 1.2 spec.
//!
//! Build/run:  `just xhciscan`  (graphical)  /  `just xhciscan-serial`.

#![no_std]
#![no_main]

#[macro_use]
extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams, ScopedProtocol};
use uefi::prelude::*;
use uefi::proto::pci::PciIoAddress;
use uefi::proto::pci::root_bridge::PciRootBridgeIo;

/// PCI class/subclass/prog-if for an xHCI (USB 3) host controller.
const XHCI_CLASS: u8 = 0x0c;
const XHCI_SUBCLASS: u8 = 0x03;
const XHCI_PROGIF: u8 = 0x30;

/// Extended-capability IDs we care about (xHCI spec table 7-1).
const XCAP_SUPPORTED_PROTOCOL: u8 = 0x02;
const XCAP_DEBUG_CAPABILITY: u8 = 0x0a;

#[entry]
fn main() -> Status {
    uefi::helpers::init().unwrap();
    let report = build_report();
    system::with_stdout(|out| {
        let _ = out.clear();
        for line in &report {
            let _ = writeln!(out, "{line}\r");
        }
        let _ = writeln!(out, "\r\npress a key to exit...\r");
    });
    wait_for_key();
    Status::SUCCESS
}

/// Locate the first xHCI controller and produce a human-readable report.
fn build_report() -> Vec<String> {
    let mut r = Vec::new();
    r.push(String::from(
        "ferrite-uefi :: xhciscan  -  USB host controller recon",
    ));
    r.push(String::from(
        "-------------------------------------------------------------------",
    ));

    let Ok(handles) = boot::find_handles::<PciRootBridgeIo>() else {
        r.push(String::from("no PCI root bridge protocol available"));
        return r;
    };

    for handle in handles {
        let params = OpenProtocolParams {
            handle,
            agent: boot::image_handle(),
            controller: None,
        };
        let Ok(mut rb) = (unsafe {
            boot::open_protocol::<PciRootBridgeIo>(params, OpenProtocolAttributes::GetProtocol)
        }) else {
            continue;
        };
        let Ok(tree) = rb.enumerate() else { continue };
        let addrs: Vec<_> = tree.iter().copied().collect();
        for addr in addrs {
            if is_xhci(&mut rb, addr) {
                report_controller(&mut rb, addr, &mut r);
                return r; // first controller is enough for the demo
            }
        }
    }

    r.push(String::from(
        "no xHCI controller found (add `-device qemu-xhci` in QEMU)",
    ));
    r
}

fn is_xhci(rb: &mut ScopedProtocol<PciRootBridgeIo>, addr: PciIoAddress) -> bool {
    let Some(reg0) = cfg_rd(rb, addr, 0x00) else {
        return false;
    };
    if (reg0 & 0xffff) as u16 == 0xffff {
        return false;
    }
    let Some(reg_class) = cfg_rd(rb, addr, 0x08) else {
        return false;
    };
    let class = ((reg_class >> 24) & 0xff) as u8;
    let subclass = ((reg_class >> 16) & 0xff) as u8;
    let progif = ((reg_class >> 8) & 0xff) as u8;
    class == XHCI_CLASS && subclass == XHCI_SUBCLASS && progif == XHCI_PROGIF
}

/// Read the controller's capability registers, port states, and extended caps.
fn report_controller(
    rb: &mut ScopedProtocol<PciRootBridgeIo>,
    addr: PciIoAddress,
    r: &mut Vec<String>,
) {
    let reg0 = cfg_rd(rb, addr, 0x00).unwrap_or(0);
    let vendor = (reg0 & 0xffff) as u16;
    let device = (reg0 >> 16) as u16;
    r.push(format!(
        "xHCI at {:02x}:{:02x}.{}  {:04x}:{:04x}",
        addr.bus, addr.dev, addr.fun, vendor, device
    ));

    // BAR0 is the xHCI MMIO base (a 64-bit memory BAR on real parts).
    let bar0_lo = cfg_rd(rb, addr, 0x10).unwrap_or(0);
    let base = if !(bar0_lo & 0x1 != 0) && (bar0_lo & 0x6) == 0x4 {
        let hi = cfg_rd(rb, addr, 0x14).unwrap_or(0);
        (u64::from(hi) << 32) | u64::from(bar0_lo & 0xffff_fff0)
    } else {
        u64::from(bar0_lo & 0xffff_fff0)
    };
    if base == 0 {
        r.push(String::from(
            "  BAR0 is zero - controller not memory-mapped",
        ));
        return;
    }
    r.push(format!("  MMIO base (BAR0): {base:#012x}"));

    // Ensure Memory Space decode is enabled before reading MMIO.
    let reg_cmd = cfg_rd(rb, addr, 0x04).unwrap_or(0);
    if reg_cmd & 0x2 == 0 {
        let _ = rb
            .pci()
            .write_one::<u32>(addr.with_register(0x04), reg_cmd | 0x2);
    }

    // Capability registers (base-relative).
    let cap0 = mrd(rb, base + 0x00).unwrap_or(0);
    let caplength = (cap0 & 0xff) as u64;
    let hci_version = (cap0 >> 16) as u16;
    let hcs1 = mrd(rb, base + 0x04).unwrap_or(0);
    let max_ports = ((hcs1 >> 24) & 0xff) as u8;
    let hcc1 = mrd(rb, base + 0x10).unwrap_or(0);
    let ppc = hcc1 & (1 << 3) != 0; // Port Power Control supported?
    let xecp = (hcc1 >> 16) & 0xffff; // extended-cap pointer, in dwords

    r.push(format!(
        "  HCIVERSION {:x}.{:02x}   CAPLENGTH {:#x}   MaxPorts {}",
        hci_version >> 8,
        hci_version & 0xff,
        caplength,
        max_ports
    ));
    r.push(format!(
        "  Port Power Control (PPC): {}   {}",
        yes_no(ppc),
        if ppc {
            "=> PORTSC.PP can toggle VBUS from software (1-bit TX line)"
        } else {
            "=> port power is fixed; no software VBUS toggle"
        }
    ));

    // Operational registers begin at base + CAPLENGTH; per-port PORTSC blocks
    // start at op_base + 0x400, each 0x10 bytes.
    let op_base = base + caplength;
    r.push(String::from("  ports:"));
    for port in 0..max_ports {
        let portsc_addr = op_base + 0x400 + u64::from(port) * 0x10;
        let Some(portsc) = mrd(rb, portsc_addr) else {
            continue;
        };
        let ccs = portsc & 0x1 != 0; // current connect status
        let ped = portsc & 0x2 != 0; // port enabled
        let pp = portsc & (1 << 9) != 0; // port power
        let pls = (portsc >> 5) & 0xf; // link state
        let speed = (portsc >> 10) & 0xf;
        r.push(format!(
            "    port {:>2}: PORTSC={:#010x}  connected={}  enabled={}  powered={}  link={}  speed={}",
            port + 1,
            portsc,
            yes_no(ccs),
            yes_no(ped),
            yes_no(pp),
            link_state(pls),
            speed_name(speed),
        ));
    }

    // Walk the extended capability list looking for Supported Protocol + DbC.
    report_ext_caps(rb, base, xecp, r);
}

/// Traverse the xHCI extended-capability linked list.
fn report_ext_caps(
    rb: &mut ScopedProtocol<PciRootBridgeIo>,
    base: u64,
    xecp_dwords: u32,
    r: &mut Vec<String>,
) {
    if xecp_dwords == 0 {
        r.push(String::from("  extended caps: none"));
        return;
    }
    let mut off = base + u64::from(xecp_dwords) * 4;
    let mut dbc_found = false;
    let mut guard = 0;
    loop {
        guard += 1;
        if guard > 64 {
            break; // malformed list backstop
        }
        let Some(cap) = mrd(rb, off) else { break };
        let id = (cap & 0xff) as u8;
        let next = ((cap >> 8) & 0xff) as u64; // in dwords, 0 = end

        match id {
            XCAP_DEBUG_CAPABILITY => {
                dbc_found = true;
                r.push(format!(
                    "  ext cap @ {off:#x}: id 0x0a DEBUG CAPABILITY (DbC) <== USB-serial channel!"
                ));
            }
            XCAP_SUPPORTED_PROTOCOL => {
                // Bytes: name string at +4 ("USB "), major/minor rev in cap dword.
                let rev_minor = (cap >> 16) & 0xff;
                let rev_major = (cap >> 24) & 0xff;
                let name = mrd(rb, off + 4).unwrap_or(0);
                let n = name.to_le_bytes();
                r.push(format!(
                    "  ext cap @ {off:#x}: id 0x02 supported protocol {}{}{}{} {}.{}",
                    ascii(n[0]),
                    ascii(n[1]),
                    ascii(n[2]),
                    ascii(n[3]),
                    rev_major,
                    rev_minor
                ));
            }
            other => {
                r.push(format!("  ext cap @ {off:#x}: id {other:#04x}"));
            }
        }

        if next == 0 {
            break;
        }
        off += next * 4;
    }

    r.push(format!(
        "  Debug Capability (DbC): {}   {}",
        yes_no(dbc_found),
        if dbc_found {
            "=> a real bidirectional USB-serial link is possible (crossover cable)"
        } else {
            "=> not exposed here (QEMU's model omits it; real HW should have it)"
        }
    ));
}

fn cfg_rd(rb: &mut ScopedProtocol<PciRootBridgeIo>, addr: PciIoAddress, off: u8) -> Option<u32> {
    rb.pci().read_one::<u32>(addr.with_register(off)).ok()
}

/// Read a 32-bit value from the controller's MMIO space.
fn mrd(rb: &mut ScopedProtocol<PciRootBridgeIo>, phys: u64) -> Option<u32> {
    rb.memory().read_one::<u32>(phys).ok()
}

const fn yes_no(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

const fn ascii(b: u8) -> char {
    if b.is_ascii_graphic() { b as char } else { '.' }
}

const fn link_state(pls: u32) -> &'static str {
    match pls {
        0 => "U0",
        1 => "U1",
        2 => "U2",
        3 => "U3",
        4 => "Disabled",
        5 => "RxDetect",
        6 => "Inactive",
        7 => "Polling",
        8 => "Recovery",
        9 => "HotReset",
        10 => "Compliance",
        11 => "TestMode",
        15 => "Resume",
        _ => "?",
    }
}

const fn speed_name(s: u32) -> &'static str {
    match s {
        0 => "-",
        1 => "Full",
        2 => "Low",
        3 => "High",
        4 => "Super",
        5 => "Super+",
        _ => "?",
    }
}

fn wait_for_key() {
    system::with_stdin(|input| {
        if let Ok(event) = input.wait_for_key_event() {
            let _ = boot::wait_for_event(&mut [event]);
        }
    });
}
