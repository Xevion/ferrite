//! The dashboard's colours: a dark ground, one cool accent, amber for a pause, and red kept for
//! faults.

#![expect(clippy::unreadable_literal, reason = "colours read as #rrggbb")]

use crate::gfx::Rgb;

pub const PAGE: Rgb = Rgb::hex(0x090c11);
pub const CARD: Rgb = Rgb::hex(0x131a22);
pub const CARD_EDGE: Rgb = Rgb::hex(0x222c38);
pub const CHIP: Rgb = Rgb::hex(0x1b2430);
pub const ROW: Rgb = Rgb::hex(0x1a2531);
pub const TRACK: Rgb = Rgb::hex(0x0b0f14);

pub const TEXT: Rgb = Rgb::hex(0xe8edf2);
pub const MUTED: Rgb = Rgb::hex(0x8e9aa9);
pub const DIM: Rgb = Rgb::hex(0x4f5a68);

pub const ACCENT: Rgb = Rgb::hex(0x38bdf8);
pub const ACCENT_FROM: Rgb = Rgb::hex(0x2dd4bf);
pub const GOOD: Rgb = Rgb::hex(0x3fb950);
pub const WARN: Rgb = Rgb::hex(0xf2b33d);
pub const BAD: Rgb = Rgb::hex(0xff5f5f);
pub const BAD_DEEP: Rgb = Rgb::hex(0xc92a3a);
pub const BAD_GROUND: Rgb = Rgb::hex(0x34121a);
pub const BAD_EDGE: Rgb = Rgb::hex(0x8a2633);
