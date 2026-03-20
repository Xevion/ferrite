use std::fmt;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_binary_scaling() {
        assert_eq!(Size::new(0.0, UnitSystem::Binary).to_string(), "0.00 B");
        assert_eq!(Size::new(512.0, UnitSystem::Binary).to_string(), "512 B");
        assert_eq!(
            Size::new(1024.0, UnitSystem::Binary).to_string(),
            "1.00 KiB"
        );
        assert_eq!(
            Size::new(1024.0 * 1024.0, UnitSystem::Binary).to_string(),
            "1.00 MiB"
        );
        assert_eq!(
            Size::new(1024.0 * 1024.0 * 1024.0, UnitSystem::Binary).to_string(),
            "1.00 GiB"
        );
        assert_eq!(
            Size::new(1536.0 * 1024.0, UnitSystem::Binary).to_string(),
            "1.50 MiB"
        );
    }

    #[test]
    fn size_decimal_scaling() {
        assert_eq!(
            Size::new(1000.0, UnitSystem::Decimal).to_string(),
            "1.00 KB"
        );
        assert_eq!(
            Size::new(1_000_000.0, UnitSystem::Decimal).to_string(),
            "1.00 MB"
        );
        assert_eq!(
            Size::new(1_000_000_000.0, UnitSystem::Decimal).to_string(),
            "1.00 GB"
        );
    }

    #[test]
    fn rate_binary_scaling() {
        let rate = Rate::new(10.0 * 1024.0 * 1024.0 * 1024.0, UnitSystem::Binary);
        assert_eq!(rate.to_string(), "10.0 GiB/s");
    }

    #[test]
    fn rate_decimal_scaling() {
        let rate = Rate::new(25.0 * 1_000_000_000.0, UnitSystem::Decimal);
        assert_eq!(rate.to_string(), "25.0 GB/s");
    }

    #[test]
    fn explicit_precision() {
        let size = Size::new(1536.0 * 1024.0, UnitSystem::Binary);
        assert_eq!(format!("{size:.1}"), "1.5 MiB");
        assert_eq!(format!("{size:.3}"), "1.500 MiB");
    }
}
