// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Utility type.

use std::ops::Index;
use std::ops::IndexMut;

/// A `Vec` whose indexes don't change as elements are added and removed.
#[derive(Debug)]
pub struct SparseVec<T>(Vec<Option<T>>);

impl<T> Default for SparseVec<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> SparseVec<T> {
    /// Creates a new sparse vector.
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Adds an entry, returning its index.
    pub fn add(&mut self, t: T) -> usize {
        let (index, entry) = match self.0.iter_mut().enumerate().find(|(_, x)| x.is_none()) {
            Some((index, entry)) => (index, entry),
            None => {
                let index = self.0.len();
                (index, self.0.push_mut(None))
            }
        };
        *entry = Some(t);
        index
    }

    /// Removes an entry by index.
    ///
    /// # Panics
    ///
    /// Panics if `index` was not added or has already been removed.
    pub fn remove(&mut self, index: usize) -> T {
        self.0[index].take().unwrap()
    }

    /// Returns an iterator for the entries.
    pub fn iter(&self) -> impl Iterator<Item = (usize, &'_ T)> {
        self.0
            .iter()
            .enumerate()
            .filter_map(|(i, v)| v.as_ref().map(|v| (i, v)))
    }

    /// Returns a mutable iterator for the entries.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (usize, &'_ mut T)> {
        self.0
            .iter_mut()
            .enumerate()
            .filter_map(|(i, v)| v.as_mut().map(|v| (i, v)))
    }
}

impl<T> Index<usize> for SparseVec<T> {
    type Output = T;

    fn index(&self, index: usize) -> &Self::Output {
        self.0.get(index).and_then(Option::as_ref).unwrap()
    }
}

impl<T> IndexMut<usize> for SparseVec<T> {
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        self.0.get_mut(index).and_then(Option::as_mut).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::SparseVec;
    use test_with_tracing::test;

    #[test]
    fn add_appends_entries() {
        let mut entries = SparseVec::new();
        assert_eq!(entries.add(String::from("first")), 0);
        assert_eq!(entries.add(String::from("second")), 1);
        assert_eq!(entries[0], "first");
        assert_eq!(entries[1], "second");
    }

    #[test]
    fn add_reuses_first_vacancy() {
        let mut entries = SparseVec::new();
        assert_eq!(entries.add(String::from("first")), 0);
        assert_eq!(entries.add(String::from("second")), 1);
        assert_eq!(entries.add(String::from("third")), 2);
        assert_eq!(entries.remove(0), "first");
        assert_eq!(entries.remove(2), "third");
        assert_eq!(entries.add(String::from("replacement")), 0);
        assert_eq!(entries[0], "replacement");
        assert_eq!(entries[1], "second");
        assert_eq!(entries.add(String::from("another")), 2);
        assert_eq!(entries[2], "another");
    }
}
