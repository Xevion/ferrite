//! Model-specific register access: the trait a platform implements, and typed register addresses.

use core::fmt;
use core::marker::PhantomData;

/// An access the processor refused with a general-protection fault: the register does not exist
/// on this processor, or the written value set a reserved bit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fault {
    pub address: u32,
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MSR {:#x} faulted", self.address)
    }
}

impl core::error::Error for Fault {}

/// A register's typed value and its raw 64 bits.
pub trait Register: Copy {
    fn from_raw(raw: u64) -> Self;
    fn to_raw(self) -> u64;
}

impl Register for u64 {
    fn from_raw(raw: u64) -> Self {
        raw
    }

    fn to_raw(self) -> u64 {
        self
    }
}

/// Implements [`Register`] for bitbybit structs over `u64`.
macro_rules! register {
    ($($t:ty),* $(,)?) => {
        $(impl $crate::msr::Register for $t {
            fn from_raw(raw: u64) -> Self {
                Self::new_with_raw_value(raw)
            }

            fn to_raw(self) -> u64 {
                self.raw_value()
            }
        })*
    };
}
pub(crate) use register;

/// A register's address, paired with the type its value decodes to.
pub struct Msr<T> {
    address: u32,
    value: PhantomData<fn() -> T>,
}

impl<T> Msr<T> {
    #[must_use]
    pub const fn at(address: u32) -> Self {
        Self { address, value: PhantomData }
    }

    #[must_use]
    pub const fn address(self) -> u32 {
        self.address
    }

    /// The register `by` addresses on, for register arrays such as AMD's P-state definitions.
    #[must_use]
    pub const fn offset(self, by: u32) -> Self {
        Self::at(self.address + by)
    }
}

impl<T> Clone for Msr<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Msr<T> {}

impl<T> fmt::Debug for Msr<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Msr({:#x})", self.address)
    }
}

/// MSR access on the core that calls it. Implementations turn the processor's fault on a missing
/// register into [`Fault`] rather than letting it take the machine down.
pub trait MsrIo {
    fn read_raw(&mut self, address: u32) -> Result<u64, Fault>;
    fn write_raw(&mut self, address: u32, value: u64) -> Result<(), Fault>;

    fn read<T: Register>(&mut self, msr: Msr<T>) -> Result<T, Fault> {
        self.read_raw(msr.address).map(T::from_raw)
    }

    fn write<T: Register>(&mut self, msr: Msr<T>, value: T) -> Result<(), Fault> {
        self.write_raw(msr.address, value.to_raw())
    }
}

impl<M: MsrIo + ?Sized> MsrIo for &mut M {
    fn read_raw(&mut self, address: u32) -> Result<u64, Fault> {
        (**self).read_raw(address)
    }

    fn write_raw(&mut self, address: u32, value: u64) -> Result<(), Fault> {
        (**self).write_raw(address, value)
    }
}
