//! The dashboard's colours: the "Aptio Gray" scheme of a BIOS setup screen, a blue title bar over a
//! grey page with dark blue ink.

#![expect(clippy::unreadable_literal, reason = "colours read as #rrggbb")]

use crate::gfx::Rgb;

pub const BG: Rgb = Rgb::hex(0xc8ccd4);
pub const LINE: Rgb = Rgb::hex(0x7a8396);
pub const TRACK: Rgb = Rgb::hex(0xe6e9ef);

pub const TEXT: Rgb = Rgb::hex(0x1b2a5c);
pub const STRONG: Rgb = Rgb::hex(0x0a1238);
pub const DIM: Rgb = Rgb::hex(0x3e4a6b);
pub const LABEL: Rgb = Rgb::hex(0x1e40af);

pub const BAR: Rgb = Rgb::hex(0x1e3a8a);
pub const BAR_TEXT: Rgb = Rgb::hex(0xffffff);
pub const BAR_DIM: Rgb = Rgb::hex(0xc7d2fe);
pub const BAR_RULE: Rgb = Rgb::hex(0x3b5bb5);
pub const TAB_TEXT: Rgb = Rgb::hex(0xdce3f7);
pub const TAB_ON: Rgb = BG;
pub const TAB_ON_INK: Rgb = STRONG;

pub const FILL: Rgb = LABEL;
pub const SEL: Rgb = BAR;
pub const SEL_INK: Rgb = Rgb::hex(0xffffff);
pub const PASS: Rgb = Rgb::hex(0x14532d);
pub const ERR: Rgb = Rgb::hex(0x991b1b);

pub const TESTED: Rgb = Rgb::hex(0x3f8f62);
pub const PENDING: Rgb = Rgb::hex(0xa7aebd);
pub const HELD: Rgb = Rgb::hex(0x6e7585);
pub const ACTIVE: Rgb = LABEL;
pub const ACTIVE_RING: Rgb = STRONG;

pub const KEY: Rgb = Rgb::hex(0xffffff);
pub const KEY_INK: Rgb = BAR;
