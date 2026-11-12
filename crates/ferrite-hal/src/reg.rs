//! Typed 32-bit register descriptors.
//!
//! A [`Reg`] pairs an offset with its value type and an access marker. Plain
//! read-write registers take any value; registers holding write-one-to-clear
//! fields only accept an [`Rw1cWrite`], which the value type's named helpers
//! build, so writing back a value as read cannot clear status by accident.

use core::fmt;
use core::marker::PhantomData;

use crate::{BusAddr32, Mmio, PhysAddr, PlatformError};

/// A register's decoded value, convertible to and from the raw dword.
pub trait RegValue: Copy {
    fn from_raw(raw: u32) -> Self;
    fn to_raw(self) -> u32;
}

impl RegValue for u32 {
    fn from_raw(raw: u32) -> Self {
        raw
    }

    fn to_raw(self) -> u32 {
        self
    }
}

/// Register that is only read.
#[derive(Clone, Copy, Debug)]
pub struct ReadOnly;
/// Register whose writable fields are all plain read-write.
#[derive(Clone, Copy, Debug)]
pub struct ReadWrite;
/// Register with write-one-to-clear (or write-one-to-set) fields.
#[derive(Clone, Copy, Debug)]
pub struct Rw1c;

mod sealed {
    pub trait Sealed {}
    impl Sealed for super::ReadOnly {}
    impl Sealed for super::ReadWrite {}
    impl Sealed for super::Rw1c {}
}

/// Access markers that permit reads.
pub trait Readable: sealed::Sealed {}
impl Readable for ReadOnly {}
impl Readable for ReadWrite {}
impl Readable for Rw1c {}

/// A 32-bit register at `offset` from its block's base, holding a `T`.
pub struct Reg<T, A> {
    offset: u64,
    marker: PhantomData<fn() -> (T, A)>,
}

impl<T, A> Clone for Reg<T, A> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T, A> Copy for Reg<T, A> {}

impl<T, A> fmt::Debug for Reg<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Reg({:#x})", self.offset)
    }
}

impl<T, A> Reg<T, A> {
    #[must_use]
    pub const fn new(offset: u64) -> Self {
        Self { offset, marker: PhantomData }
    }

    #[must_use]
    pub const fn offset(self) -> u64 {
        self.offset
    }

    /// Absolute address within the block at `base`.
    ///
    /// # Panics
    /// If the address overflows, which only a corrupt base can cause.
    #[must_use]
    pub const fn addr(self, base: PhysAddr) -> PhysAddr {
        base.offset(self.offset)
    }
}

/// A value for a [`Rw1c`] register whose write-one-to-clear bits were chosen on purpose.
pub struct Rw1cWrite<T> {
    raw: u32,
    marker: PhantomData<fn() -> T>,
}

impl<T> Clone for Rw1cWrite<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Rw1cWrite<T> {}

impl<T> fmt::Debug for Rw1cWrite<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Rw1cWrite({:#010x})", self.raw)
    }
}

impl<T> PartialEq for Rw1cWrite<T> {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}

impl<T> Eq for Rw1cWrite<T> {}

impl<T: RegValue> Rw1cWrite<T> {
    /// Write `value` exactly: every write-one-to-clear bit set in it is cleared.
    /// Register types call this only from their named acknowledge helpers.
    #[must_use]
    pub fn clearing(value: T) -> Self {
        Self { raw: value.to_raw(), marker: PhantomData }
    }

    #[must_use]
    pub const fn raw(self) -> u32 {
        self.raw
    }
}

/// A 64-bit DMA pointer register, written only from a [`BusAddr32`].
#[derive(Clone, Copy, Debug)]
pub struct DmaPtrReg {
    offset: u64,
}

impl DmaPtrReg {
    #[must_use]
    pub const fn new(offset: u64) -> Self {
        Self { offset }
    }

    /// # Panics
    /// As [`Reg::addr`].
    #[must_use]
    pub const fn addr(self, base: PhysAddr) -> PhysAddr {
        base.offset(self.offset)
    }
}

/// Typed register access for every [`Mmio`] backend.
pub trait MmioExt: Mmio {
    fn read<T: RegValue, A: Readable>(&mut self, base: PhysAddr, reg: Reg<T, A>) -> T {
        T::from_raw(self.read32(reg.addr(base)))
    }

    fn try_read<T: RegValue, A: Readable>(&mut self, base: PhysAddr, reg: Reg<T, A>) -> Result<T, PlatformError> {
        self.try_read32(reg.addr(base)).map(T::from_raw)
    }

    fn write<T: RegValue>(&mut self, base: PhysAddr, reg: Reg<T, ReadWrite>, value: T) {
        self.write32(reg.addr(base), value.to_raw());
    }

    fn try_write<T: RegValue>(
        &mut self,
        base: PhysAddr,
        reg: Reg<T, ReadWrite>,
        value: T,
    ) -> Result<(), PlatformError> {
        self.try_write32(reg.addr(base), value.to_raw())
    }

    fn write_rw1c<T: RegValue>(&mut self, base: PhysAddr, reg: Reg<T, Rw1c>, value: Rw1cWrite<T>) {
        self.write32(reg.addr(base), value.raw());
    }

    fn try_write_rw1c<T: RegValue>(
        &mut self,
        base: PhysAddr,
        reg: Reg<T, Rw1c>,
        value: Rw1cWrite<T>,
    ) -> Result<(), PlatformError> {
        self.try_write32(reg.addr(base), value.raw())
    }

    /// Both halves of a 64-bit register, low dword first.
    fn read_dma_ptr(&mut self, base: PhysAddr, reg: DmaPtrReg) -> u64 {
        let addr = reg.addr(base);
        let lo = u64::from(self.read32(addr));
        lo | (u64::from(self.read32(addr.offset(4))) << 32)
    }

    /// Low dword first, then a zero high dword: never torn, since the high half never changes.
    fn write_dma_ptr(&mut self, base: PhysAddr, reg: DmaPtrReg, value: BusAddr32) {
        let addr = reg.addr(base);
        self.write32(addr, value.get());
        self.write32(addr.offset(4), 0);
    }

    fn try_write_dma_ptr(&mut self, base: PhysAddr, reg: DmaPtrReg, value: BusAddr32) -> Result<(), PlatformError> {
        let addr = reg.addr(base);
        self.try_write32(addr, value.get())?;
        self.try_write32(addr.offset(4), 0)
    }
}

impl<M: Mmio + ?Sized> MmioExt for M {}
