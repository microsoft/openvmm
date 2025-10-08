// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Sector bitmaps for tracking read sectors during layered read operations.
//!
//! FUTURE: use real bitmaps instead of bool vectors.

use std::ops::Range;

pub(crate) struct Bitmap {
    bits: Vec<bool>,
    sector: u64,
}

impl Bitmap {
    pub fn new(sector: u64, len: usize) -> Self {
        Self {
            bits: vec![false; len],
            sector,
        }
    }

    pub fn unset_iter(&mut self) -> impl Iterator<Item = SectorBitmapRange<'_>> {
        let mut n = 0;
        let sector = self.sector;
        self.bits
            .chunk_by_mut(|&a, &b| a == b)
            .filter_map(move |bits| {
                let start = n;
                n += bits.len();
                if bits.first().is_some_and(|&x| !x) {
                    Some(SectorBitmapRange {
                        bits,
                        start_sector: sector + start as u64,
                        sector_within_bitmap: start,
                        set_count: 0,
                    })
                } else {
                    None
                }
            })
    }
}

pub(crate) struct SectorBitmapRange<'a> {
    bits: &'a mut [bool],
    start_sector: u64,
    sector_within_bitmap: usize,
    set_count: usize,
}

impl SectorBitmapRange<'_> {
    pub fn view(&mut self, len: u64) -> SectorMarker<'_> {
        SectorMarker {
            bits: &mut self.bits[..len as usize],
            set_count: &mut self.set_count,
            sector_base: self.start_sector,
        }
    }

    pub fn start_sector_within_bitmap(&self) -> usize {
        self.sector_within_bitmap
    }

    pub fn start_sector(&self) -> u64 {
        self.start_sector
    }

    pub fn end_sector(&self) -> u64 {
        self.start_sector + self.bits.len() as u64
    }

    pub fn len(&self) -> u64 {
        self.bits.len() as u64
    }

    pub fn set_count(&self) -> usize {
        self.set_count
    }

    pub(crate) fn unset_iter(&self) -> impl '_ + Iterator<Item = Range<u64>> {
        let mut n = self.start_sector;
        self.bits.chunk_by(|&a, &b| a == b).filter_map(move |bits| {
            let start = n;
            n += bits.len() as u64;
            if bits.first().is_some_and(|&x| !x) {
                Some(start..n)
            } else {
                None
            }
        })
    }
}

/// A type to mark sectors that have been read by a layer as part of a
/// [`LayerIo::read`](super::LayerIo::read) operation.
pub struct SectorMarker<'a> {
    bits: &'a mut [bool],
    sector_base: u64,
    set_count: &'a mut usize,
}

impl SectorMarker<'_> {
    #[track_caller]
    fn sector_to_index(&self, sector: u64) -> usize {
        let i = sector
            .checked_sub(self.sector_base)
            .expect("invalid sector");
        assert!(i < self.bits.len() as u64, "invalid sector");
        i as usize
    }

    /// Mark the specified sector number as having been read.
    #[track_caller]
    pub fn set(&mut self, sector: u64) {
        let i = self.sector_to_index(sector);
        *self.set_count += !self.bits[i] as usize;
        self.bits[i] = true;
    }

    /// Mark the range of sectors as having been read.
    #[track_caller]
    pub fn set_range(&mut self, range: Range<u64>) {
        for sector in range {
            self.set(sector);
        }
    }

    /// Mark all the sectors as having been read.
    pub fn set_all(&mut self) {
        self.set_range(self.sector_base..self.sector_base + self.bits.len() as u64);
    }
}

#[cfg(test)]
mod tests {
    use super::Bitmap;

    #[test]
    fn test_bitmap() {
        let base = 0x492893;
        let mut bitmap = Bitmap::new(base, 10);
        {
            let mut iter = bitmap.unset_iter();
            let mut range = iter.next().unwrap();
            assert!(iter.next().is_none());
            assert_eq!(range.start_sector(), base);
            assert_eq!(range.end_sector(), base + 10);
            assert_eq!(range.len(), 10);
            assert_eq!(range.set_count(), 0);
            range.view(6).set_all();
            assert_eq!(range.set_count(), 6);
        }
        {
            let mut iter = bitmap.unset_iter();
            let mut range = iter.next().unwrap();
            assert!(iter.next().is_none());
            assert_eq!(range.start_sector(), base + 6);
            assert_eq!(range.end_sector(), base + 10);
            assert_eq!(range.start_sector_within_bitmap(), 6);
            assert_eq!(range.len(), 4);
            range.view(4).set(base + 7);
            assert_eq!(range.set_count(), 1);
        }
        {
            let mut iter = bitmap.unset_iter();
            let range = iter.next().unwrap();
            let range2 = iter.next().unwrap();
            assert!(iter.next().is_none());
            assert_eq!(range.start_sector(), base + 6);
            assert_eq!(range.end_sector(), base + 7);
            assert_eq!(range2.start_sector(), base + 8);
            assert_eq!(range2.end_sector(), base + 10);
        }
    }
}
