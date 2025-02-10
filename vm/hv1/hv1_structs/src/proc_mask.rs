// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Structures for working with processor masks.

/// A set of processor IDs, stored as a sparse array of 64-bit masks.
#[derive(Clone)]
pub struct ProcessorSet<'a> {
    valid_masks: u64,
    masks: &'a [u64],
}

impl<'a> ProcessorSet<'a> {
    pub fn from_generic_set(format: u64, rest: &'a [u64]) -> Option<Self> {
        if format != hvdef::hypercall::HV_GENERIC_SET_SPARSE_4K {
            return None;
        }
        let &[valid_masks, ref masks @ ..] = rest else {
            return None;
        };
        Self::from_processor_masks(valid_masks, masks)
    }

    pub fn from_processor_masks(valid_masks: u64, masks: &'a [u64]) -> Option<Self> {
        let mask_count = valid_masks.count_ones();
        if masks.len() != mask_count as usize {
            return None;
        }
        Some(Self { valid_masks, masks })
    }

    /// Returns the set as a raw HV_GENERIC_SET_SPARSE_4K, suitable for use in a hypercall.
    pub fn as_raw(&self) -> Vec<u64> {
        let mut raw = Vec::with_capacity(1 + self.masks.len());
        raw.push(self.valid_masks);
        raw.extend_from_slice(self.masks);
        raw
    }

    pub fn is_empty(&self) -> bool {
        self.valid_masks == 0 || self.masks.iter().all(|x| *x == 0)
    }

    pub fn len(&self) -> usize {
        self.masks.iter().map(|x| x.count_ones() as usize).sum()
    }
}

impl<'a> IntoIterator for &'a ProcessorSet<'a> {
    type Item = u32;
    type IntoIter = ProcessorSetIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        ProcessorSetIter {
            len: self.len(),
            set: self,
            valid_mask_bit: 0,
            mask_idx: 0,
            mask_bit: 0,
        }
    }
}

pub struct ProcessorSetIter<'a> {
    set: &'a ProcessorSet<'a>,
    valid_mask_bit: usize,
    mask_idx: usize,
    mask_bit: usize,
    len: usize,
}

impl Iterator for ProcessorSetIter<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<Self::Item> {
        while self.valid_mask_bit < 64 {
            if self.set.valid_masks & (1 << self.valid_mask_bit) == 0 {
                self.valid_mask_bit += 1;
                continue;
            }
            let mask = self.set.masks[self.mask_idx];
            while self.mask_bit < 64 {
                if mask & (1 << self.mask_bit) == 0 {
                    self.mask_bit += 1;
                    continue;
                }
                let processor_id = (self.valid_mask_bit * 64 + self.mask_bit) as u32;
                self.mask_bit += 1;
                return Some(processor_id);
            }
            self.mask_bit = 0;
            self.mask_idx += 1;
            self.valid_mask_bit += 1;
        }
        None
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.len, Some(self.len))
    }
}

impl ExactSizeIterator for ProcessorSetIter<'_> {}

impl std::iter::FusedIterator for ProcessorSetIter<'_> {}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    // Values taken from the Hypervisor Functional Specification
    fn test_processor_set() {
        let set = ProcessorSet::from_processor_masks(0x5, &[0x21, 0x4]).unwrap();
        assert_eq!(set.len(), 3);

        let mut iter = set.into_iter();
        assert_eq!(iter.next(), Some(0));
        assert_eq!(iter.next(), Some(5));
        assert_eq!(iter.next(), Some(130));
        assert_eq!(iter.next(), None);
    }
}
