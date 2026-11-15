//! A set of fieldless enum variants, one bit each.

use core::fmt;
use core::marker::PhantomData;

/// An enum that can be a member of a [`Set`]: at most 32 variants.
pub trait Flag: Copy + strum::VariantArray + Into<&'static str> + 'static {
    /// The variant's bit, its declaration order.
    fn index(self) -> u32;
}

/// Implements [`Flag`] for fieldless enums by declaration order.
macro_rules! flag {
    ($($t:ty),* $(,)?) => {
        $(impl $crate::set::Flag for $t {
            fn index(self) -> u32 {
                self as u32
            }
        })*
    };
}
pub(crate) use flag;

pub struct Set<T> {
    bits: u32,
    of: PhantomData<T>,
}

impl<T: Flag> Set<T> {
    pub const EMPTY: Self = Self { bits: 0, of: PhantomData };

    #[must_use]
    pub fn has(self, flag: T) -> bool {
        self.bits & (1 << flag.index()) != 0
    }

    #[must_use]
    pub fn with(self, flag: T) -> Self {
        Self { bits: self.bits | 1 << flag.index(), of: PhantomData }
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.bits == 0
    }

    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self { bits: self.bits | other.bits, of: PhantomData }
    }

    pub fn iter(self) -> impl Iterator<Item = T> {
        T::VARIANTS.iter().copied().filter(move |&f| self.has(f))
    }
}

impl<T: Flag> FromIterator<T> for Set<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        iter.into_iter().fold(Self::EMPTY, Self::with)
    }
}

impl<T> Clone for Set<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Set<T> {}

impl<T> PartialEq for Set<T> {
    fn eq(&self, other: &Self) -> bool {
        self.bits == other.bits
    }
}

impl<T> Eq for Set<T> {}

impl<T: Flag> Default for Set<T> {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl<T: Flag> fmt::Debug for Set<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter().map(Into::<&'static str>::into)).finish()
    }
}

/// The members' names separated by spaces, or "none".
impl<T: Flag> fmt::Display for Set<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut names = self.iter().map(Into::<&'static str>::into);
        match names.next() {
            None => f.write_str("none"),
            Some(first) => {
                f.write_str(first)?;
                names.try_for_each(|name| write!(f, " {name}"))
            },
        }
    }
}
