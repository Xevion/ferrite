//! The processor's clock, energy and limit registers on Intel and AMD, over any MSR access.
//!
//! Everything here is pure: a platform supplies CPUID ([`cpuid::CpuidSource`]) and model-specific
//! registers ([`msr::MsrIo`]), and this crate decides what exists, decodes it into typed values and
//! encodes clock requests. [`discover::Platform::discover`] is the entry point: read-only, and it
//! names what it found and why anything was left out.
//!
//! The register layouts follow the Intel SDM and AMD PPRs as Linux's `intel_pstate`, `amd-pstate`,
//! `intel_rapl`, perf RAPL, `cpupower` and `turbostat` use them, and EDK2's MSR headers. Where those
//! disagree across generations the decoder is chosen by model, and a model not in a table gets no
//! decoder rather than a guess.

#![no_std]

#[cfg(test)]
extern crate std;

pub mod amd;
pub mod control;
pub mod cpuid;
pub mod discover;
pub mod intel;
pub mod limits;
pub mod msr;
pub mod rapl;
pub mod set;
pub mod telemetry;
pub mod units;

#[cfg(test)]
mod fake;
#[cfg(test)]
mod tests;
