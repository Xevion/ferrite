//! UEFI status codes as the loader reports them, with their spec names.

use std::fmt;

const ERROR_BIT: u64 = 1 << 63;

/// (code without the error bit, name) for the errors the specification defines.
const ERRORS: [(u64, &str); 35] = [
    (1, "EFI_LOAD_ERROR"),
    (2, "EFI_INVALID_PARAMETER"),
    (3, "EFI_UNSUPPORTED"),
    (4, "EFI_BAD_BUFFER_SIZE"),
    (5, "EFI_BUFFER_TOO_SMALL"),
    (6, "EFI_NOT_READY"),
    (7, "EFI_DEVICE_ERROR"),
    (8, "EFI_WRITE_PROTECTED"),
    (9, "EFI_OUT_OF_RESOURCES"),
    (10, "EFI_VOLUME_CORRUPTED"),
    (11, "EFI_VOLUME_FULL"),
    (12, "EFI_NO_MEDIA"),
    (13, "EFI_MEDIA_CHANGED"),
    (14, "EFI_NOT_FOUND"),
    (15, "EFI_ACCESS_DENIED"),
    (16, "EFI_NO_RESPONSE"),
    (17, "EFI_NO_MAPPING"),
    (18, "EFI_TIMEOUT"),
    (19, "EFI_NOT_STARTED"),
    (20, "EFI_ALREADY_STARTED"),
    (21, "EFI_ABORTED"),
    (22, "EFI_ICMP_ERROR"),
    (23, "EFI_TFTP_ERROR"),
    (24, "EFI_PROTOCOL_ERROR"),
    (25, "EFI_INCOMPATIBLE_VERSION"),
    (26, "EFI_SECURITY_VIOLATION"),
    (27, "EFI_CRC_ERROR"),
    (28, "EFI_END_OF_MEDIA"),
    (31, "EFI_END_OF_FILE"),
    (32, "EFI_INVALID_LANGUAGE"),
    (33, "EFI_COMPROMISED_DATA"),
    (34, "EFI_IP_ADDRESS_CONFLICT"),
    (35, "EFI_HTTP_ERROR"),
    (36, "EFI_NETWORK_UNREACHABLE"),
    (37, "EFI_HOST_UNREACHABLE"),
];

/// Warnings carry no error bit.
const WARNINGS: [(u64, &str); 7] = [
    (1, "EFI_WARN_UNKNOWN_GLYPH"),
    (2, "EFI_WARN_DELETE_FAILURE"),
    (3, "EFI_WARN_WRITE_FAILURE"),
    (4, "EFI_WARN_BUFFER_TOO_SMALL"),
    (5, "EFI_WARN_STALE_DATA"),
    (6, "EFI_WARN_FILE_SYSTEM"),
    (7, "EFI_WARN_RESET_REQUIRED"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EfiStatus {
    Success,
    Named { code: u64, name: &'static str },
    Unknown(u64),
}

impl EfiStatus {
    #[must_use]
    pub fn from_code(code: u64) -> Self {
        if code == 0 {
            return Self::Success;
        }
        let (table, number) =
            if code & ERROR_BIT == 0 { (&WARNINGS[..], code) } else { (&ERRORS[..], code & !ERROR_BIT) };
        table
            .iter()
            .find(|&&(known, _)| known == number)
            .map_or(Self::Unknown(code), |&(_, name)| Self::Named { code, name })
    }

    #[must_use]
    pub const fn is_success(self) -> bool {
        matches!(self, Self::Success)
    }
}

impl fmt::Display for EfiStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Success => f.write_str("EFI_SUCCESS (0)"),
            Self::Named { code, name } => write!(f, "{name} ({code:#x})"),
            Self::Unknown(code) => write!(f, "unknown status {code:#x}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::success(0, "EFI_SUCCESS (0)")]
    #[case::aborted(0x8000_0000_0000_0015, "EFI_ABORTED (0x8000000000000015)")]
    #[case::unsupported(0x8000_0000_0000_0003, "EFI_UNSUPPORTED (0x8000000000000003)")]
    #[case::warning(5, "EFI_WARN_STALE_DATA (0x5)")]
    #[case::unassigned_error(0x8000_0000_0000_0063, "unknown status 0x8000000000000063")]
    #[case::unassigned_warning(0x42, "unknown status 0x42")]
    fn codes_are_named_with_the_error_bit_taken_into_account(#[case] code: u64, #[case] want: &str) {
        check!(EfiStatus::from_code(code).to_string() == want);
    }
}
