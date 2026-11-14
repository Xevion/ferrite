//! The NTP packet (RFC 5905), as the client mode of SNTP (RFC 4330) uses it: 48 bytes, big-endian.

use core::fmt;

use ferrite_hal::UnixTime;

/// The UDP port NTP servers listen on.
pub const PORT: u16 = 123;
/// The size of a packet without extension fields or a MAC.
pub const PACKET_LEN: usize = 48;
/// Seconds from 1900-01-01, where NTP time starts, to 1970-01-01.
const UNIX_EPOCH_IN_NTP: i64 = 2_208_988_800;
const ERA: i64 = 1 << 32;

/// An NTP timestamp: seconds since 1900 and a binary fraction of a second.
///
/// The seconds are the high 32 bits and the fraction the low 32. The seconds wrap every 136 years (the
/// next time in 2036), so a timestamp alone does not say which era it is in: [`Timestamp::to_unix`]
/// takes a pivot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timestamp(u64);

impl Timestamp {
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn new(seconds: u32, fraction: u32) -> Self {
        Self(((seconds as u64) << 32) | fraction as u64)
    }

    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }

    #[must_use]
    pub const fn seconds(self) -> u32 {
        (self.0 >> 32) as u32
    }

    #[must_use]
    pub const fn fraction(self) -> u32 {
        self.0 as u32
    }

    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// The seconds of `utc`, modulo the era, with no fraction.
    #[must_use]
    pub fn from_unix_seconds(utc: UnixTime) -> Self {
        let seconds = i64::try_from(utc.since_epoch().as_secs()).unwrap_or(0).saturating_add(UNIX_EPOCH_IN_NTP);
        Self::new(u32::try_from(seconds.rem_euclid(ERA)).unwrap_or(0), 0)
    }

    /// This instant as UTC, in the era that puts it nearest `pivot`: any `pivot` within 68 years of the
    /// true time picks the right one. `None` for a time before 1970.
    #[must_use]
    pub fn to_unix(self, pivot: UnixTime) -> Option<UnixTime> {
        let pivot = i64::try_from(pivot.since_epoch().as_secs()).unwrap_or(i64::MAX / 2);
        let era_zero = i64::from(self.seconds()) - UNIX_EPOCH_IN_NTP;
        let eras = (pivot - era_zero + ERA / 2).div_euclid(ERA);
        let seconds = u64::try_from(era_zero.checked_add(eras.checked_mul(ERA)?)?).ok()?;
        let nanos = (u64::from(self.fraction()) * 1_000_000_000) >> 32;
        Some(UnixTime::from_epoch(core::time::Duration::new(seconds, nanos as u32)))
    }
}

/// The two leap indicator bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Leap {
    None,
    /// The last minute of the day has 61 seconds.
    Insert,
    /// The last minute of the day has 59 seconds.
    Delete,
    /// The server's clock is not synchronized.
    Alarm,
}

impl Leap {
    const fn from_raw(raw: u8) -> Self {
        match raw & 0b11 {
            0 => Self::None,
            1 => Self::Insert,
            2 => Self::Delete,
            _ => Self::Alarm,
        }
    }
}

/// The three mode bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    SymmetricActive,
    SymmetricPassive,
    Client,
    Server,
    Broadcast,
    Control,
    Private,
    Unknown(u8),
}

impl Mode {
    const fn from_raw(raw: u8) -> Self {
        match raw {
            1 => Self::SymmetricActive,
            2 => Self::SymmetricPassive,
            3 => Self::Client,
            4 => Self::Server,
            5 => Self::Broadcast,
            6 => Self::Control,
            7 => Self::Private,
            other => Self::Unknown(other),
        }
    }

    const fn raw(self) -> u8 {
        match self {
            Self::SymmetricActive => 1,
            Self::SymmetricPassive => 2,
            Self::Client => 3,
            Self::Server => 4,
            Self::Broadcast => 5,
            Self::Control => 6,
            Self::Private => 7,
            Self::Unknown(raw) => raw & 0b111,
        }
    }
}

/// The stratum byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stratum {
    /// 0: not a time but a kiss-o'-death, whose code is in the reference identifier.
    Kiss,
    /// 1 for a server with a reference clock, up to 15 for the farthest from one.
    Server(u8),
    /// 16: the server is not synchronized.
    Unsynchronized,
    Unknown(u8),
}

impl Stratum {
    const fn from_raw(raw: u8) -> Self {
        match raw {
            0 => Self::Kiss,
            1..=15 => Self::Server(raw),
            16 => Self::Unsynchronized,
            other => Self::Unknown(other),
        }
    }

    const fn raw(self) -> u8 {
        match self {
            Self::Kiss => 0,
            Self::Server(raw) | Self::Unknown(raw) => raw,
            Self::Unsynchronized => 16,
        }
    }
}

/// What a kiss-o'-death says, from the four ASCII characters of the reference identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kiss {
    /// Too many requests: back off.
    Rate,
    /// The server will not serve this client.
    Deny,
    /// The same, by policy.
    Rstr,
    Unknown([u8; 4]),
}

impl Kiss {
    #[must_use]
    pub const fn from_code(code: [u8; 4]) -> Self {
        match &code {
            b"RATE" => Self::Rate,
            b"DENY" => Self::Deny,
            b"RSTR" => Self::Rstr,
            _ => Self::Unknown(code),
        }
    }

    /// Whether the client must stop using this server.
    #[must_use]
    pub const fn bans(self) -> bool {
        !matches!(self, Self::Unknown(_))
    }
}

impl fmt::Display for Kiss {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rate => f.write_str("RATE"),
            Self::Deny => f.write_str("DENY"),
            Self::Rstr => f.write_str("RSTR"),
            Self::Unknown(code) => code
                .iter()
                .try_for_each(|&b| fmt::Write::write_char(f, if b.is_ascii_graphic() { char::from(b) } else { '?' })),
        }
    }
}

/// An NTP packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Packet {
    pub leap: Leap,
    pub version: u8,
    pub mode: Mode,
    pub stratum: Stratum,
    pub poll: i8,
    pub precision: i8,
    pub root_delay: u32,
    pub root_dispersion: u32,
    pub reference_id: [u8; 4],
    pub reference: Timestamp,
    /// The client's transmit timestamp as the server read it.
    pub origin: Timestamp,
    /// When the request arrived at the server.
    pub receive: Timestamp,
    /// When the reply left the server.
    pub transmit: Timestamp,
}

/// Why bytes are not a packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TooShort {
    pub len: usize,
}

impl Packet {
    /// A client request in version 4 whose transmit timestamp is `transmit`.
    #[must_use]
    pub const fn request(transmit: Timestamp) -> Self {
        Self {
            leap: Leap::None,
            version: 4,
            mode: Mode::Client,
            stratum: Stratum::Kiss,
            poll: 0,
            precision: 0,
            root_delay: 0,
            root_dispersion: 0,
            reference_id: [0; 4],
            reference: Timestamp::ZERO,
            origin: Timestamp::ZERO,
            receive: Timestamp::ZERO,
            transmit,
        }
    }

    #[must_use]
    pub fn encode(&self) -> [u8; PACKET_LEN] {
        let leap = match self.leap {
            Leap::None => 0,
            Leap::Insert => 1,
            Leap::Delete => 2,
            Leap::Alarm => 3,
        };
        let mut out = [0; PACKET_LEN];
        out[0] = leap << 6 | (self.version & 0b111) << 3 | self.mode.raw();
        out[1] = self.stratum.raw();
        out[2] = self.poll.cast_unsigned();
        out[3] = self.precision.cast_unsigned();
        out[4..8].copy_from_slice(&self.root_delay.to_be_bytes());
        out[8..12].copy_from_slice(&self.root_dispersion.to_be_bytes());
        out[12..16].copy_from_slice(&self.reference_id);
        for (at, time) in [(16, self.reference), (24, self.origin), (32, self.receive), (40, self.transmit)] {
            out[at..at + 8].copy_from_slice(&time.raw().to_be_bytes());
        }
        out
    }

    /// The packet at the start of `bytes`; anything after its 48 bytes (extension fields, a MAC) is
    /// not read.
    pub fn parse(bytes: &[u8]) -> Result<Self, TooShort> {
        let Some(head) = bytes.first_chunk::<PACKET_LEN>() else {
            return Err(TooShort { len: bytes.len() });
        };
        let be32 = |at: usize| u32::from_be_bytes([head[at], head[at + 1], head[at + 2], head[at + 3]]);
        let time = |at: usize| Timestamp::new(be32(at), be32(at + 4));
        Ok(Self {
            leap: Leap::from_raw(head[0] >> 6),
            version: head[0] >> 3 & 0b111,
            mode: Mode::from_raw(head[0] & 0b111),
            stratum: Stratum::from_raw(head[1]),
            poll: head[2].cast_signed(),
            precision: head[3].cast_signed(),
            root_delay: be32(4),
            root_dispersion: be32(8),
            reference_id: [head[12], head[13], head[14], head[15]],
            reference: time(16),
            origin: time(24),
            receive: time(32),
            transmit: time(40),
        })
    }
}
