//! TRB, ERST and DbC context layouts with their encoders. Pure data, no MMIO.
//!
//! The DMA-resident structs keep their `repr(C)` layout over plain dwords; the
//! bitfield types are views for encoding and decoding individual dwords.

use arbitrary_int::{u3, u5, u6, u17, u24};
use bitbybit::bitfield;

use ferrite_hal::{BusAddr, BusAddr32};

use crate::regs::{CompletionCode, Ddi1, Ddi2, TrbType};

pub const EP_TYPE_BULK_OUT: u3 = u3::new(2);
pub const EP_TYPE_BULK_IN: u3 = u3::new(6);
/// Bulk max packet size at `SuperSpeed`, and the size of each IN receive buffer.
pub const MAX_PACKET: usize = 1024;

pub const DBC_VENDOR_ID: u16 = 0x1d6b;
pub const DBC_PRODUCT_ID: u16 = 0x0010;
pub const DBC_PROTOCOL: u8 = 1;
pub const DBC_DEVICE_REV: u16 = 0x0010;

/// Longest string (in characters) that fits a 64 byte descriptor slot.
pub const MAX_STRING_CHARS: usize = 31;
/// Bytes per string descriptor slot in the string table.
const STRING_SLOT: usize = 64;

/// TRB dword 3: cycle, type and the per-type flags this driver uses.
#[bitfield(u32, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct TrbControl {
    /// Transfer events: endpoint context index.
    #[bits(16..=20, rw)]
    endpoint_id: u5,
    #[bits(10..=15, rw)]
    trb_type: Option<TrbType>,
    #[bits(10..=15, r)]
    trb_type_raw: u6,
    /// Interrupt On Completion.
    #[bit(5, rw)]
    ioc: bool,
    /// Link TRBs: toggle the ring's cycle state when followed.
    #[bit(1, rw)]
    toggle_cycle: bool,
    #[bit(0, rw)]
    cycle: bool,
}

/// Normal TRB dword 2.
#[bitfield(u32, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct NormalStatus {
    #[bits(0..=16, rw)]
    transfer_len: u17,
}

/// Transfer event dword 2.
#[bitfield(u32, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct EventStatus {
    #[bits(24..=31, rw)]
    completion_code: Option<CompletionCode>,
    #[bits(24..=31, r)]
    completion_code_raw: u8,
    /// Bytes not transferred.
    #[bits(0..=23, rw)]
    residual: u24,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C, align(16))]
pub struct Trb {
    pub d: [u32; 4],
}

impl Trb {
    /// Normal TRB with IOC, to be queued with its cycle bit set by the ring.
    #[must_use]
    pub const fn normal(buf: BusAddr32, len: u17) -> Self {
        let status = NormalStatus::DEFAULT.with_transfer_len(len);
        let control = TrbControl::DEFAULT.with_trb_type(TrbType::Normal).with_ioc(true);
        Self { d: [buf.get(), 0, status.raw_value(), control.raw_value()] }
    }

    /// Link TRB back to `next` with toggle-cycle set and the cycle bit clear.
    #[must_use]
    pub const fn link(next: BusAddr32) -> Self {
        let control = TrbControl::DEFAULT.with_trb_type(TrbType::Link).with_toggle_cycle(true);
        Self { d: [next.get(), 0, 0, control.raw_value()] }
    }

    #[must_use]
    pub const fn control(&self) -> TrbControl {
        TrbControl::new_with_raw_value(self.d[3])
    }

    #[must_use]
    pub const fn event_status(&self) -> EventStatus {
        EventStatus::new_with_raw_value(self.d[2])
    }

    #[must_use]
    pub const fn cycle(&self) -> bool {
        self.control().cycle()
    }

    pub const fn set_cycle(&mut self, cycle: bool) {
        self.d[3] = self.control().with_cycle(cycle).raw_value();
    }

    /// Data pointer; in an event, the TRB the event refers to.
    #[must_use]
    pub const fn pointer(&self) -> BusAddr {
        BusAddr::new((self.d[0] as u64) | ((self.d[1] as u64) << 32))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct ErstEntry {
    pub base: u64,
    pub size: u32,
    pub reserved: u32,
}

impl ErstEntry {
    #[must_use]
    pub const fn new(base: BusAddr32, trbs: u32) -> Self {
        Self { base: base.get() as u64, size: trbs, reserved: 0 }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct InfoContext {
    pub string0: u64,
    pub manufacturer: u64,
    pub product: u64,
    pub serial: u64,
    pub length: u32,
    pub reserved: [u32; 7],
}

/// Endpoint context dword 1.
#[bitfield(u32, default = 0, debug)]
#[derive(PartialEq, Eq)]
pub struct EpInfo2 {
    #[bits(16..=31, rw)]
    max_packet: u16,
    #[bits(8..=15, rw)]
    max_burst: u8,
    #[bits(3..=5, rw)]
    ep_type: u3,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct EpContext {
    pub info1: u32,
    pub info2: u32,
    pub deq: u64,
    pub tx_info: u32,
    pub reserved: [u32; 11],
}

impl EpContext {
    /// Bulk endpoint whose ring starts at `ring` with cycle state 1.
    #[must_use]
    pub const fn bulk(ep_type: u3, max_burst: u8, ring: BusAddr32) -> Self {
        let info2 = EpInfo2::DEFAULT.with_ep_type(ep_type).with_max_burst(max_burst).with_max_packet(MAX_PACKET as u16);
        Self { info1: 0, info2: info2.raw_value(), deq: ring.get() as u64 | 1, tx_info: 0, reserved: [0; 11] }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Context {
    pub info: InfoContext,
    pub out: EpContext,
    pub in_ep: EpContext,
}

const _: () = {
    assert!(size_of::<Trb>() == 16);
    assert!(size_of::<ErstEntry>() == 16);
    assert!(size_of::<InfoContext>() == 64);
    assert!(size_of::<EpContext>() == 64);
    assert!(size_of::<Context>() == 192);
    assert!(size_of::<StringTable>() == 4 * STRING_SLOT);
};

/// The four string descriptors in 64 byte slots, plus the packed lengths the
/// info context carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct StringTable {
    pub slots: [[u8; STRING_SLOT]; 4],
}

impl StringTable {
    pub const STRING0: usize = 0;
    pub const MANUFACTURER: usize = 1;
    pub const PRODUCT: usize = 2;
    pub const SERIAL: usize = 3;

    /// Build the table; strings longer than `MAX_STRING_CHARS` are truncated and
    /// non-ASCII bytes are not supported.
    #[must_use]
    pub fn new(manufacturer: &str, product: &str, serial: &str) -> Self {
        let mut slots = [[0u8; STRING_SLOT]; 4];
        slots[Self::STRING0][..4].copy_from_slice(&[4, 3, 0x09, 0x04]);
        Self::put(&mut slots[Self::MANUFACTURER], manufacturer);
        Self::put(&mut slots[Self::PRODUCT], product);
        Self::put(&mut slots[Self::SERIAL], serial);
        Self { slots }
    }

    fn put(slot: &mut [u8; STRING_SLOT], s: &str) {
        let chars = s.len().min(MAX_STRING_CHARS);
        slot[0] = (2 + 2 * chars) as u8;
        slot[1] = 3;
        for (i, b) in s.bytes().take(chars).enumerate() {
            slot[2 + 2 * i] = b;
            slot[3 + 2 * i] = 0;
        }
    }

    /// Info context `length` field: string0 in byte 0 up to serial in byte 3.
    #[must_use]
    pub fn packed_lengths(&self) -> u32 {
        let len = |i: usize| u32::from(self.slots[i][0]);
        (len(Self::SERIAL) << 24) | (len(Self::PRODUCT) << 16) | (len(Self::MANUFACTURER) << 8) | len(Self::STRING0)
    }
}

impl Context {
    /// Full context for a DbC whose strings live at `strings_dma` (a `StringTable`).
    ///
    /// # Panics
    /// If the string table would reach past 4 GiB.
    #[must_use]
    pub fn new(
        strings: &StringTable,
        strings_dma: BusAddr32,
        max_burst: u8,
        out_ring: BusAddr32,
        in_ring: BusAddr32,
    ) -> Self {
        let slot = |i: usize| {
            let addr = strings_dma
                .checked_add((STRING_SLOT * i) as u32)
                .unwrap_or_else(|| panic!("string table reaches 4 GiB"));
            u64::from(addr.get())
        };
        Self {
            info: InfoContext {
                string0: slot(StringTable::STRING0),
                manufacturer: slot(StringTable::MANUFACTURER),
                product: slot(StringTable::PRODUCT),
                serial: slot(StringTable::SERIAL),
                length: strings.packed_lengths(),
                reserved: [0; 7],
            },
            out: EpContext::bulk(EP_TYPE_BULK_OUT, max_burst, out_ring),
            in_ep: EpContext::bulk(EP_TYPE_BULK_IN, max_burst, in_ring),
        }
    }
}

#[must_use]
pub const fn ddi1() -> Ddi1 {
    Ddi1::DEFAULT.with_vendor(DBC_VENDOR_ID).with_protocol(DBC_PROTOCOL)
}

#[must_use]
pub const fn ddi2() -> Ddi2 {
    Ddi2::DEFAULT.with_device_rev(DBC_DEVICE_REV).with_product(DBC_PRODUCT_ID)
}

#[cfg(test)]
mod tests {
    use assert2::check;
    use rstest::rstest;

    use super::*;

    /// Encoder output that real hardware accepted.
    #[rstest]
    #[case::normal(Trb::normal(BusAddr32::from_u32(0x2345_6780), u17::new(1024)).d, [0x2345_6780, 0, 1024, (1 << 10) | (1 << 5)])]
    #[case::link(Trb::link(BusAddr32::from_u32(0x8000)).d, [0x8000, 0, 0, (6 << 10) | 2])]
    #[case::bulk_out(ep_dwords(EP_TYPE_BULK_OUT, 15, 0x4000), [0, (2 << 3) | (15 << 8) | (1024 << 16), 0x4001, 0])]
    #[case::bulk_in(ep_dwords(EP_TYPE_BULK_IN, 0, 0x6000), [0, (6 << 3) | (1024 << 16), 0x6001, 0])]
    fn golden_dwords(#[case] got: [u32; 4], #[case] want: [u32; 4]) {
        check!(got == want);
    }

    fn ep_dwords(ep_type: u3, max_burst: u8, ring: u32) -> [u32; 4] {
        let ep = EpContext::bulk(ep_type, max_burst, BusAddr32::from_u32(ring));
        [ep.info1, ep.info2, ep.deq as u32, (ep.deq >> 32) as u32]
    }

    #[test]
    fn device_info_matches_linux_values() {
        check!(ddi1().raw_value() == 0x1d6b_0001);
        check!(ddi2().raw_value() == 0x0010_0010);
    }

    #[test]
    fn string_table_layout() {
        let t = StringTable::new("ferrite", "ferrite DbC", "0001");
        check!(t.slots[0][..4] == [4, 3, 9, 4]);
        check!(t.slots[1][0] == 2 + 2 * 7);
        check!(t.slots[3][..4] == [10, 3, b'0', 0]);
        check!(t.packed_lengths() == (10 << 24) | (24 << 16) | (16 << 8) | 4);
    }

    #[rstest]
    #[case::fits(31, 2 + 2 * 31)]
    #[case::truncated(32, 2 + 2 * 31)]
    #[case::far_over(200, 2 + 2 * 31)]
    fn string_truncates_at_31_chars(#[case] len: usize, #[case] want_len: u8) {
        let long: std::string::String = core::iter::repeat_n('x', len).collect();
        let t = StringTable::new(&long, "", "");
        let slot = t.slots[StringTable::MANUFACTURER];
        check!(slot[0] == want_len);
        check!(slot[usize::from(want_len) - 2] == b'x');
        check!(slot[usize::from(want_len)..].iter().all(|&b| b == 0), "nothing written past the descriptor");
    }
}
