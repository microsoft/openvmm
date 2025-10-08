// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Structures for working with processor masks.

/// A set of processor IDs, stored as a sparse array of 64-bit masks.
#[derive(Copy, Clone)]
pub struct ProcessorSet<'a> {
    valid_masks: u64,
    masks: &'a [u64],
}

impl<'a> ProcessorSet<'a> {
    /// Attempts to create a ProcessorSet from a HV_GENERIC_SET_SPARSE_4K format HV_GENERIC_SET.
    pub fn from_generic_set(format: u64, rest: &'a [u64]) -> Option<Self> {
        if format != hvdef::hypercall::HV_GENERIC_SET_SPARSE_4K {
            return None;
        }
        let &[valid_masks, ref masks @ ..] = rest else {
            return None;
        };
        Self::from_processor_masks(valid_masks, masks)
    }

    /// Attempts to create a ProcessorSet from a set of processor masks.
    pub fn from_processor_masks(valid_masks: u64, masks: &'a [u64]) -> Option<Self> {
        let mask_count = valid_masks.count_ones();
        if masks.len() != mask_count as usize {
            return None;
        }
        Some(Self { valid_masks, masks })
    }

    /// Returns the set as an iterator of u64s, suitable for collecting and
    /// using as raw HV_GENERIC_SET_SPARSE_4K in a hypercall.
    pub fn as_generic_set(&self) -> impl Iterator<Item = u64> + use<'_> {
        std::iter::once(self.valid_masks).chain(self.masks.iter().copied())
    }

    /// Returns true if the set is empty.
    pub fn is_empty(&self) -> bool {
        self.valid_masks == 0 || self.count() == 0
    }

    /// Returns the number of processors in the set.
    pub fn count(&self) -> usize {
        self.masks.iter().map(|x| x.count_ones() as usize).sum()
    }

    /// Returns an iterator over the processor IDs in the set.
    pub fn iter(&self) -> ProcessorSetIter<'a> {
        self.into_iter()
    }
}

impl<'a> IntoIterator for ProcessorSet<'a> {
    type Item = u32;
    type IntoIter = ProcessorSetIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        ProcessorSetIter {
            bit: 0,
            mask: 0,
            remaining_valid: self.valid_masks,
            masks: self.masks,
        }
    }
}

/// An iterator over the processor IDs in a ProcessorSet.
pub struct ProcessorSetIter<'a> {
    bit: u32,
    mask: u64,
    remaining_valid: u64,
    masks: &'a [u64],
}

impl Iterator for ProcessorSetIter<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        while self.mask == 0 {
            self.mask = *self.masks.first()?;
            self.masks = &self.masks[1..];
            self.bit = self.remaining_valid.trailing_zeros();
            self.remaining_valid &= !(1 << self.bit);
        }
        let proc = self.mask.trailing_zeros();
        self.mask &= !(1 << proc);
        Some(self.bit * 64 + proc)
    }
}

impl std::iter::FusedIterator for ProcessorSetIter<'_> {}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    // Values taken from the Hypervisor Functional Specification
    fn test_processor_set() {
        let set = ProcessorSet::from_processor_masks(0x5, &[0x21, 0x4]).unwrap();
        assert_eq!(set.count(), 3);

        let mut iter = set.into_iter();
        assert_eq!(iter.next(), Some(0));
        assert_eq!(iter.next(), Some(5));
        assert_eq!(iter.next(), Some(130));
        assert_eq!(iter.next(), None);
    }
}
