pub mod alloc;
pub mod dimm;
pub mod edac;
pub mod error_analysis;
pub mod failure;
pub mod output;
pub mod pattern;
pub mod phys;
pub mod runner;
pub mod simd;
pub mod smbios;
#[cfg(feature = "tui")]
pub mod tui;
pub mod units;

pub use alloc::CompactionGuard;
pub use failure::Failure;
