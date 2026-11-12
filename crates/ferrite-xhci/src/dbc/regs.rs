//! DbC and host root-port register offsets and bits (xHCI 1.2, sections 5.4.8 and 7.6.8).

/// Offsets relative to the DbC extended capability (ID 0x0a).
pub const DCID: u64 = 0x00;
pub const DCDB: u64 = 0x04;
pub const DCERSTSZ: u64 = 0x08;
pub const DCERSTBA: u64 = 0x10;
pub const DCERDP: u64 = 0x18;
pub const DCCTRL: u64 = 0x20;
pub const DCST: u64 = 0x24;
pub const DCPORTSC: u64 = 0x28;
pub const DCCP: u64 = 0x30;
pub const DCDDI1: u64 = 0x38;
pub const DCDDI2: u64 = 0x3c;

pub const CTRL_DCR: u32 = 1 << 0;
pub const CTRL_LSE: u32 = 1 << 1;
pub const CTRL_HOT: u32 = 1 << 2;
pub const CTRL_HIT: u32 = 1 << 3;
pub const CTRL_DRC: u32 = 1 << 4;
pub const CTRL_DCE: u32 = 1 << 31;

#[must_use]
pub const fn ctrl_max_burst(ctrl: u32) -> u8 {
    ((ctrl >> 16) & 0xff) as u8
}

#[must_use]
pub const fn ctrl_dev_addr(ctrl: u32) -> u8 {
    ((ctrl >> 24) & 0x7f) as u8
}

#[must_use]
pub const fn status_debug_port(status: u32) -> u8 {
    (status >> 24) as u8
}

/// Doorbell targets.
pub const DB_OUT: u32 = 0;
pub const DB_IN: u32 = 1;

/// PORTSC bits shared by DCPORTSC and the host's root-port registers.
pub const PORTSC_CCS: u32 = 1 << 0;
pub const PORTSC_PED: u32 = 1 << 1;
pub const PORTSC_PR: u32 = 1 << 4;
pub const PORTSC_PP: u32 = 1 << 9;
pub const PORTSC_CSC: u32 = 1 << 17;
pub const PORTSC_PRC: u32 = 1 << 21;
pub const PORTSC_PLC: u32 = 1 << 22;
pub const PORTSC_CEC: u32 = 1 << 23;
pub const PORTSC_CHANGES: u32 = PORTSC_CSC | PORTSC_PRC | PORTSC_PLC | PORTSC_CEC;
/// Host PORTSC bits safe to write back: PP and the wake-enable bits. Everything
/// else is RW1C, status, or a write-ignored field.
pub const PORTSC_PRESERVE: u32 = PORTSC_PP | (0x7 << 25);

pub const HOST_PORTSC_BASE: u64 = 0x400;
pub const HOST_PORT_STRIDE: u64 = 0x10;

pub const XCAP_LEGACY: u8 = 0x01;
pub const XCAP_SUPPORTED_PROTOCOL: u8 = 0x02;
pub const XCAP_DEBUG: u8 = 0x0a;

pub const LEGACY_BIOS_OWNED: u32 = 1 << 16;
pub const LEGACY_OS_OWNED: u32 = 1 << 24;
/// Legacy control bits that are reserved and kept as read; the rest are SMI enables.
pub const LEGACY_CTL_KEEP: u32 = (0x3 << 1) + (0xff << 5) + (0x7 << 17);
/// Legacy control status bits that clear when written as one.
pub const LEGACY_CTL_EVENTS: u32 = 0x7 << 29;

#[must_use]
pub const fn link_state_name(pls: u32) -> &'static str {
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

#[must_use]
pub const fn speed_name(s: u32) -> &'static str {
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

#[must_use]
pub const fn trb_type_name(t: u32) -> &'static str {
    match t {
        1 => "Normal",
        6 => "Link",
        32 => "TransferEvent",
        33 => "CommandCompletion",
        34 => "PortStatusChange",
        37 => "HostController",
        _ => "other",
    }
}
