use std::fmt;

/// Format a byte count as a size string reversible by `parse_size`.
///
/// Picks the largest exact unit: `G` if divisible by 1 GiB, `M` if divisible
/// by 1 MiB, `K` if divisible by 1 KiB, plain decimal otherwise.
#[must_use]
pub fn format_size(bytes: usize) -> String {
    const GIB: usize = 1024 * 1024 * 1024;
    const MIB: usize = 1024 * 1024;
    const KIB: usize = 1024;
    if bytes.is_multiple_of(GIB) {
        format!("{}G", bytes / GIB)
    } else if bytes.is_multiple_of(MIB) {
        format!("{}M", bytes / MIB)
    } else if bytes.is_multiple_of(KIB) {
        format!("{}K", bytes / KIB)
    } else {
        format!("{bytes}")
    }
}

/// Whether to display sizes in binary (KiB, MiB, GiB) or decimal (KB, MB, GB) units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum UnitSystem {
    #[default]
    Binary,
    Decimal,
}

const BINARY_SUFFIXES: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
const DECIMAL_SUFFIXES: &[&str] = &["B", "KB", "MB", "GB", "TB", "PB"];

/// A byte count paired with a unit system for display.
#[derive(Debug, Clone, Copy)]
pub struct Size {
    pub bytes: f64,
    pub system: UnitSystem,
}

impl Size {
    pub fn new(bytes: impl Into<f64>, system: UnitSystem) -> Self {
        Self {
            bytes: bytes.into(),
            system,
        }
    }
}

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (divisor, suffixes) = match self.system {
            UnitSystem::Binary => (1024.0_f64, BINARY_SUFFIXES),
            UnitSystem::Decimal => (1000.0_f64, DECIMAL_SUFFIXES),
        };

        let mut value = self.bytes;
        for (i, suffix) in suffixes.iter().enumerate() {
            if value < divisor || i == suffixes.len() - 1 {
                // Use the precision from the formatter if specified, otherwise default.
                return if let Some(precision) = f.precision() {
                    write!(f, "{value:.precision$} {suffix}")
                } else if value < 10.0 {
                    write!(f, "{value:.2} {suffix}")
                } else if value < 100.0 {
                    write!(f, "{value:.1} {suffix}")
                } else {
                    write!(f, "{value:.0} {suffix}")
                };
            }
            value /= divisor;
        }
        unreachable!()
    }
}

/// A throughput rate (bytes per second) paired with a unit system for display.
#[derive(Debug, Clone, Copy)]
pub struct Rate {
    pub bytes_per_sec: f64,
    pub system: UnitSystem,
}

impl Rate {
    #[must_use]
    pub fn new(bytes_per_sec: f64, system: UnitSystem) -> Self {
        Self {
            bytes_per_sec,
            system,
        }
    }
}

impl fmt::Display for Rate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (divisor, suffixes) = match self.system {
            UnitSystem::Binary => (1024.0_f64, BINARY_SUFFIXES),
            UnitSystem::Decimal => (1000.0_f64, DECIMAL_SUFFIXES),
        };

        let mut value = self.bytes_per_sec;
        for (i, suffix) in suffixes.iter().enumerate() {
            if value < divisor || i == suffixes.len() - 1 {
                return if let Some(precision) = f.precision() {
                    write!(f, "{value:.precision$} {suffix}/s")
                } else if value < 10.0 {
                    write!(f, "{value:.2} {suffix}/s")
                } else if value < 100.0 {
                    write!(f, "{value:.1} {suffix}/s")
                } else {
                    write!(f, "{value:.0} {suffix}/s")
                };
            }
            value /= divisor;
        }
        unreachable!()
    }
}

/// Serde helper: serialize `Duration` as fractional milliseconds (`f64`).
pub mod duration_ms {
    use std::time::Duration;

    use serde::Serializer;

    /// # Errors
    ///
    /// Returns `Err` if the serializer rejects the `f64` value.
    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_f64(d.as_secs_f64() * 1000.0)
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn size_binary_scaling() {
        check!(Size::new(0.0, UnitSystem::Binary).to_string() == "0.00 B");
        check!(Size::new(512.0, UnitSystem::Binary).to_string() == "512 B");
        check!(Size::new(1024.0, UnitSystem::Binary).to_string() == "1.00 KiB");
        check!(Size::new(1024.0 * 1024.0, UnitSystem::Binary).to_string() == "1.00 MiB");
        check!(Size::new(1024.0 * 1024.0 * 1024.0, UnitSystem::Binary).to_string() == "1.00 GiB");
        check!(Size::new(1536.0 * 1024.0, UnitSystem::Binary).to_string() == "1.50 MiB");
    }

    #[test]
    fn size_decimal_scaling() {
        check!(Size::new(1000.0, UnitSystem::Decimal).to_string() == "1.00 KB");
        check!(Size::new(1_000_000.0, UnitSystem::Decimal).to_string() == "1.00 MB");
        check!(Size::new(1_000_000_000.0, UnitSystem::Decimal).to_string() == "1.00 GB");
    }

    #[test]
    fn rate_binary_scaling() {
        let rate = Rate::new(10.0 * 1024.0 * 1024.0 * 1024.0, UnitSystem::Binary);
        check!(rate.to_string() == "10.0 GiB/s");
    }

    #[test]
    fn rate_decimal_scaling() {
        let rate = Rate::new(25.0 * 1_000_000_000.0, UnitSystem::Decimal);
        check!(rate.to_string() == "25.0 GB/s");
    }

    #[test]
    fn explicit_precision() {
        let size = Size::new(1536.0 * 1024.0, UnitSystem::Binary);
        check!(format!("{size:.1}") == "1.5 MiB");
        check!(format!("{size:.3}") == "1.500 MiB");
    }

    #[test]
    fn format_size_all_branches() {
        check!(format_size(2 * 1024 * 1024 * 1024) == "2G");
        check!(format_size(5 * 1024 * 1024) == "5M");
        check!(format_size(16 * 1024) == "16K");
        check!(format_size(999) == "999");
        check!(format_size(0) == "0G"); // 0 is divisible by anything
    }

    proptest! {
        #[test]
        fn size_display_never_panics(bytes: f64) {
            let _ = Size::new(bytes, UnitSystem::Binary).to_string();
            let _ = Size::new(bytes, UnitSystem::Decimal).to_string();
        }

        #[test]
        fn rate_display_never_panics(bytes_per_sec: f64) {
            let _ = Rate::new(bytes_per_sec, UnitSystem::Binary).to_string();
            let _ = Rate::new(bytes_per_sec, UnitSystem::Decimal).to_string();
        }
    }
}
