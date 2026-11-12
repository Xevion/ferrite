//! TRB, ERST and DbC context layouts with their encoders. Pure data, no MMIO.

pub const TRB_TYPE_NORMAL: u32 = 1;
pub const TRB_TYPE_LINK: u32 = 6;
pub const TRB_TYPE_TRANSFER_EVENT: u32 = 32;
pub const TRB_TYPE_PORT_STATUS_EVENT: u32 = 34;

pub const TRB_CYCLE: u32 = 1 << 0;
pub const TRB_LINK_TOGGLE: u32 = 1 << 1;
pub const TRB_IOC: u32 = 1 << 5;

pub const CC_SUCCESS: u8 = 1;
pub const CC_SHORT_PACKET: u8 = 13;

pub const EP_TYPE_BULK_OUT: u32 = 2;
pub const EP_TYPE_BULK_IN: u32 = 6;
pub const MAX_PACKET: u32 = 1024;

pub const DBC_VENDOR_ID: u16 = 0x1d6b;
pub const DBC_PRODUCT_ID: u16 = 0x0010;
pub const DBC_PROTOCOL: u16 = 1;
pub const DBC_DEVICE_REV: u16 = 0x0010;

/// Longest string (in characters) that fits a 64 byte descriptor slot.
pub const MAX_STRING_CHARS: usize = 31;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C, align(16))]
pub struct Trb {
    pub d: [u32; 4],
}

impl Trb {
    /// Normal TRB (type 1) with IOC, to be queued with its cycle bit set by the ring.
    #[must_use]
    pub const fn normal(buf: u64, len: u32) -> Self {
        Self {
            d: [
                buf as u32,
                (buf >> 32) as u32,
                len & 0x1_ffff,
                (TRB_TYPE_NORMAL << 10) | TRB_IOC,
            ],
        }
    }

    /// Link TRB back to `next` with toggle-cycle set and the cycle bit clear.
    #[must_use]
    pub const fn link(next: u64) -> Self {
        Self {
            d: [
                next as u32,
                (next >> 32) as u32,
                0,
                (TRB_TYPE_LINK << 10) | TRB_LINK_TOGGLE,
            ],
        }
    }

    #[must_use]
    pub const fn cycle(&self) -> bool {
        self.d[3] & TRB_CYCLE != 0
    }

    pub const fn set_cycle(&mut self, cycle: bool) {
        self.d[3] = (self.d[3] & !TRB_CYCLE) | cycle as u32;
    }

    #[must_use]
    pub const fn trb_type(&self) -> u32 {
        (self.d[3] >> 10) & 0x3f
    }

    /// Event TRBs: data pointer (the TRB a transfer event refers to).
    #[must_use]
    pub const fn pointer(&self) -> u64 {
        (self.d[0] as u64) | ((self.d[1] as u64) << 32)
    }

    /// Transfer events: completion code.
    #[must_use]
    pub const fn completion_code(&self) -> u8 {
        (self.d[2] >> 24) as u8
    }

    /// Transfer events: bytes not transferred.
    #[must_use]
    pub const fn residual(&self) -> u32 {
        self.d[2] & 0x00ff_ffff
    }

    /// Transfer events: endpoint ID (context index).
    #[must_use]
    pub const fn endpoint_id(&self) -> u8 {
        ((self.d[3] >> 16) & 0x1f) as u8
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct ErstEntry {
    pub base: u64,
    pub size: u32,
    pub reserved: u32,
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
    pub const fn bulk(ep_type: u32, max_burst: u8, ring: u64) -> Self {
        Self {
            info1: 0,
            info2: (ep_type << 3) | ((max_burst as u32) << 8) | (MAX_PACKET << 16),
            deq: ring | 1,
            tx_info: 0,
            reserved: [0; 11],
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Context {
    pub info: InfoContext,
    pub out: EpContext,
    pub inn: EpContext,
}

const _: () = {
    assert!(size_of::<Trb>() == 16);
    assert!(size_of::<ErstEntry>() == 16);
    assert!(size_of::<InfoContext>() == 64);
    assert!(size_of::<EpContext>() == 64);
    assert!(size_of::<Context>() == 192);
};

/// The four string descriptors in 64 byte slots, plus the packed lengths the
/// info context carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct StringTable {
    pub slots: [[u8; 64]; 4],
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
        let mut slots = [[0u8; 64]; 4];
        slots[Self::STRING0][..4].copy_from_slice(&[4, 3, 0x09, 0x04]);
        Self::put(&mut slots[Self::MANUFACTURER], manufacturer);
        Self::put(&mut slots[Self::PRODUCT], product);
        Self::put(&mut slots[Self::SERIAL], serial);
        Self { slots }
    }

    fn put(slot: &mut [u8; 64], s: &str) {
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
        (len(Self::SERIAL) << 24)
            | (len(Self::PRODUCT) << 16)
            | (len(Self::MANUFACTURER) << 8)
            | len(Self::STRING0)
    }
}

impl Context {
    /// Full context for a DbC whose strings live at `strings` (a `StringTable`).
    #[must_use]
    pub fn new(
        strings: &StringTable,
        strings_dma: u64,
        max_burst: u8,
        out_ring: u64,
        in_ring: u64,
    ) -> Self {
        Self {
            info: InfoContext {
                string0: strings_dma + 64 * StringTable::STRING0 as u64,
                manufacturer: strings_dma + 64 * StringTable::MANUFACTURER as u64,
                product: strings_dma + 64 * StringTable::PRODUCT as u64,
                serial: strings_dma + 64 * StringTable::SERIAL as u64,
                length: strings.packed_lengths(),
                reserved: [0; 7],
            },
            out: EpContext::bulk(EP_TYPE_BULK_OUT, max_burst, out_ring),
            inn: EpContext::bulk(EP_TYPE_BULK_IN, max_burst, in_ring),
        }
    }
}

/// DCDDI1: vendor in the high half, protocol in the low half.
#[must_use]
pub const fn ddi1() -> u32 {
    ((DBC_VENDOR_ID as u32) << 16) | DBC_PROTOCOL as u32
}

/// DCDDI2: device revision in the high half, product in the low half.
#[must_use]
pub const fn ddi2() -> u32 {
    ((DBC_DEVICE_REV as u32) << 16) | DBC_PRODUCT_ID as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_trb_encoding() {
        let trb = Trb::normal(0x1_2345_6780, 1024);
        assert_eq!(trb.d, [0x2345_6780, 1, 1024, (1 << 10) | (1 << 5)]);
        assert_eq!(trb.trb_type(), TRB_TYPE_NORMAL);
        assert!(!trb.cycle());
    }

    #[test]
    fn link_trb_toggles() {
        let trb = Trb::link(0x8000);
        assert_eq!(trb.d[3], (6 << 10) | 2);
        assert_eq!(trb.pointer(), 0x8000);
    }

    #[test]
    fn event_field_extraction() {
        let trb = Trb {
            d: [0x1000, 0, (13 << 24) | 24, (32 << 10) | (3 << 16) | 1],
        };
        assert_eq!(trb.trb_type(), TRB_TYPE_TRANSFER_EVENT);
        assert_eq!(trb.completion_code(), CC_SHORT_PACKET);
        assert_eq!(trb.residual(), 24);
        assert_eq!(trb.endpoint_id(), 3);
        assert!(trb.cycle());
    }

    #[test]
    fn device_info_matches_linux_values() {
        assert_eq!(ddi1(), 0x1d6b_0001);
        assert_eq!(ddi2(), 0x0010_0010);
    }

    #[test]
    fn endpoint_context_fields() {
        let out = EpContext::bulk(EP_TYPE_BULK_OUT, 15, 0x4000);
        assert_eq!(out.info2, (2 << 3) | (15 << 8) | (1024 << 16));
        assert_eq!(out.deq, 0x4001);
        assert_eq!(EpContext::bulk(EP_TYPE_BULK_IN, 0, 0).info2 & 0x38, 6 << 3);
    }

    #[test]
    fn string_table_layout() {
        let t = StringTable::new("ferrite", "ferrite DbC", "0001");
        assert_eq!(t.slots[0][..4], [4, 3, 9, 4]);
        assert_eq!(t.slots[1][0], 2 + 2 * 7);
        assert_eq!(t.slots[3][..4], [10, 3, b'0', 0]);
        assert_eq!(t.packed_lengths(), (10 << 24) | (24 << 16) | (16 << 8) | 4);
    }

    #[test]
    fn context_points_into_string_table() {
        let t = StringTable::new("a", "b", "c");
        let ctx = Context::new(&t, 0x2000, 3, 0x5000, 0x6000);
        assert_eq!(ctx.info.serial, 0x2000 + 192);
        assert_eq!(ctx.inn.deq, 0x6001);
    }
}
