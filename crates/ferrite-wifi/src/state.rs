use alloc::string::String;
use core::fmt;
use core::time::Duration;

use ferrite_net::port::MacAddr;

/// Why a connection attempt, or the service itself, failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The card's driver did not bind, or its interface went away.
    Driver,
    /// The scan could not run or did not finish.
    Scan,
    /// The scan found no access point offering the configured network.
    NotFound,
    /// The access point refused authentication or association.
    Rejected,
    /// Association or the 4-way handshake did not finish in time.
    Timeout,
    /// A request to the stack failed, or a handshake frame failed a check.
    Protocol,
    /// The network device could not be attached to the IP stack.
    Attach,
    /// The link dropped after it had been up: deauthentication, disassociation or beacon loss.
    Lost,
}

impl Failure {
    /// Whether trying again cannot help: the card itself is gone or never came up.
    pub const fn is_permanent(self) -> bool {
        matches!(self, Self::Driver)
    }
}

impl Failure {
    /// The failure in words, for a status line.
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Driver => "the card's driver is not running",
            Self::Scan => "the scan failed",
            Self::NotFound => "the network was not found",
            Self::Rejected => "the access point refused the connection",
            Self::Timeout => "the connection timed out",
            Self::Protocol => "the connection failed",
            Self::Attach => "the network device could not be attached",
            Self::Lost => "the link was lost",
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.describe())
    }
}

/// What the station is doing, for display. The address on a connected link is the network side's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StationState {
    /// The Linux side is coming up and the card is being brought online.
    Starting,
    /// No supported Wi-Fi card is present.
    NoCard,
    /// There is no network to join: none configured, or a `[wifi]` section that is not usable.
    NoCredentials,
    /// A firmware file the card or the regulatory domain needs is missing.
    NoFirmware,
    Scanning,
    Connecting {
        ssid: String,
    },
    /// Associated, keys installed and the port open: the link carries frames.
    Connected {
        ssid: String,
        bssid: MacAddr,
    },
    /// The last attempt failed; `retry_in` is the wait before the next one, `None` when none follows.
    Failed {
        reason: Failure,
        retry_in: Option<Duration>,
    },
    /// The service was asked to stop and has left the network.
    Stopped,
}

impl StationState {
    pub const fn is_connected(&self) -> bool {
        matches!(self, Self::Connected { .. })
    }

    /// The state without its network name, for a line drawn every frame: no formatting, no allocation.
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::NoCard => "no Wi-Fi card",
            Self::NoCredentials => "no network credentials",
            Self::NoFirmware => "firmware files missing",
            Self::Scanning => "scanning",
            Self::Connecting { .. } => "connecting",
            Self::Connected { .. } => "connected",
            Self::Failed { reason, .. } => reason.describe(),
            Self::Stopped => "stopped",
        }
    }

    /// Whether the state will never change again without a new start: the service has ended.
    pub const fn is_final(&self) -> bool {
        matches!(
            self,
            Self::NoCard | Self::NoCredentials | Self::NoFirmware | Self::Stopped | Self::Failed { retry_in: None, .. }
        )
    }
}

impl fmt::Display for StationState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Starting => f.write_str("starting"),
            Self::NoCard => f.write_str("no Wi-Fi card"),
            Self::NoCredentials => f.write_str("no network credentials"),
            Self::NoFirmware => f.write_str("firmware files missing"),
            Self::Scanning => f.write_str("scanning"),
            Self::Connecting { ssid } => write!(f, "connecting to \"{ssid}\""),
            Self::Connected { ssid, bssid } => write!(f, "connected to \"{ssid}\" ({bssid})"),
            Self::Failed { reason, retry_in: Some(wait) } => {
                write!(f, "failed: {reason}; retrying in {} s", wait.as_secs().max(1))
            },
            Self::Failed { reason, retry_in: None } => write!(f, "failed: {reason}"),
            Self::Stopped => f.write_str("stopped"),
        }
    }
}
