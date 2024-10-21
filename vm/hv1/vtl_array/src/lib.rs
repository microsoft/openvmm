// Copyright (C) Microsoft Corporation. All rights reserved.

//! Container data structures indexable by [`Vtl`].

#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use bitvec::array::BitArray;
use core::ops::Deref;
use core::ops::DerefMut;
use core::ops::Index;
use core::ops::IndexMut;
use hvdef::Vtl;
use inspect::Inspect;

// TODO: Enforce N <= 3 on the type when stable
/// An array indexable by [`Vtl`].
#[derive(Debug, Clone)]
pub struct VtlArray<T, const N: usize> {
    data: [T; N],
}

impl<T, const N: usize> VtlArray<T, N> {
    /// Creates an array of type T, where each element is `value`.
    pub const fn new(value: T) -> Self
    where
        T: Copy,
    {
        assert!(N > 0 && N <= 3);
        Self { data: [value; N] }
    }

    /// Creates an array of type T, where each element is
    /// the returned value from `f` using that element’s index.
    pub fn from_fn<F>(mut f: F) -> Self
    where
        F: FnMut(Vtl) -> T,
    {
        assert!(N > 0 && N <= 3);
        Self {
            data: core::array::from_fn(|i| f(Vtl::try_from(i as u8).unwrap())),
        }
    }
}

impl<T> From<[T; 1]> for VtlArray<T, 1> {
    fn from(a: [T; 1]) -> Self {
        Self { data: a }
    }
}

impl<T> From<[T; 2]> for VtlArray<T, 2> {
    fn from(a: [T; 2]) -> Self {
        Self { data: a }
    }
}

impl<T> From<[T; 3]> for VtlArray<T, 3> {
    fn from(a: [T; 3]) -> Self {
        Self { data: a }
    }
}

impl<T, const N: usize> Inspect for VtlArray<T, N>
where
    T: Inspect,
{
    fn inspect(&self, req: inspect::Request<'_>) {
        inspect::iter_by_index(&self.data).inspect(req)
    }
}

impl<T, V: Into<Vtl>, const N: usize> Index<V> for VtlArray<T, N> {
    type Output = T;

    fn index(&self, index: V) -> &Self::Output {
        &self.data[index.into() as usize]
    }
}

impl<T, V: Into<Vtl>, const N: usize> IndexMut<V> for VtlArray<T, N> {
    fn index_mut(&mut self, index: V) -> &mut Self::Output {
        &mut self.data[index.into() as usize]
    }
}

impl<T, const N: usize> Deref for VtlArray<T, N> {
    type Target = [T; N];

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

impl<T, const N: usize> DerefMut for VtlArray<T, N> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.data
    }
}

/// A set of [`Vtl`]s.
#[derive(Copy, Clone)]
pub struct VtlSet {
    bits: BitArray<u16>,
}

impl VtlSet {
    /// Creates a new empty set.
    pub fn new() -> Self {
        Self {
            bits: BitArray::new(0),
        }
    }

    /// Adds a [`Vtl`] to the set.
    pub fn set(&mut self, vtl: Vtl) {
        self.bits.set(vtl as usize, true);
    }

    /// Removes a [`Vtl`] from the set.
    pub fn clear(&mut self, vtl: Vtl) {
        self.bits.set(vtl as usize, false);
    }

    /// Returns true if any [`Vtl`] in the set is higher than `vtl`.
    pub fn is_higher_vtl_set_than(&self, vtl: Vtl) -> bool {
        self.highest_set() > Some(vtl)
    }

    /// Returns the highest set [`Vtl`] in the set, if any.
    pub fn highest_set(&self) -> Option<Vtl> {
        Some(Vtl::try_from(self.bits.last_one()? as u8).unwrap())
    }

    /// Returns true if the given [`Vtl`] is set.
    pub fn is_set<V: Into<Vtl>>(&self, vtl: V) -> bool {
        self.bits[vtl.into() as usize]
    }

    /// Returns true if the given [`Vtl`] is not set.
    pub fn is_clear<V: Into<Vtl>>(&self, vtl: V) -> bool {
        !self.is_set(vtl)
    }

    /// Returns an iterator over the set [`Vtl`]s, in order from highest to lowest.
    pub fn iter_highest_first(&self) -> impl Iterator<Item = Vtl> + '_ {
        self.bits
            .iter_ones()
            .rev()
            .map(|i| Vtl::try_from(i as u8).unwrap())
    }
}

impl Inspect for VtlSet {
    fn inspect(&self, req: inspect::Request<'_>) {
        inspect::iter_by_index(self.bits.iter().map(|v| *v)).inspect(req)
    }
}

impl From<u16> for VtlSet {
    fn from(bits: u16) -> Self {
        VtlSet {
            bits: BitArray::new(bits),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::VtlSet;
    use hvdef::Vtl;

    #[test]
    fn test_vtlset() {
        let mut set = VtlSet::new();
        assert_eq!(set.highest_set(), None);
        set.set(Vtl::Vtl0);
        assert_eq!(set.highest_set(), Some(Vtl::Vtl0));
        set.set(Vtl::Vtl2);
        assert_eq!(set.highest_set(), Some(Vtl::Vtl2));

        {
            let mut iter = set.iter_highest_first();
            assert_eq!(iter.next(), Some(Vtl::Vtl2));
            assert_eq!(iter.next(), Some(Vtl::Vtl0));
            assert_eq!(iter.next(), None);
        }

        assert!(!set.is_higher_vtl_set_than(Vtl::Vtl2));
        assert!(set.is_higher_vtl_set_than(Vtl::Vtl1));
        assert!(set.is_higher_vtl_set_than(Vtl::Vtl0));

        set.clear(Vtl::Vtl2);
        assert!(!set.is_higher_vtl_set_than(Vtl::Vtl0));
    }
}
