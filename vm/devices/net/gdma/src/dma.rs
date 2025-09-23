// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use gdma_defs::PAGE_SIZE32;
use gdma_defs::PAGE_SIZE64;
use guestmem::ranges::PagedRange;

#[derive(Clone)]
pub struct DmaRegion {
    /// The list of 4KB guest page numbers (GPNs).
    gpns: Vec<u64>,
    /// The starting byte offset within the first guest page.
    start: usize,
    /// The total length of the region.
    len: usize,
}

impl DmaRegion {
    pub fn new(mut gpas: Vec<u64>, start: u32, len: u64) -> anyhow::Result<Self> {
        for gpa in &mut gpas {
            if *gpa % PAGE_SIZE64 != 0 {
                anyhow::bail!("page address is not 4KB aligned");
            }
            *gpa /= PAGE_SIZE64;
        }
        if len == 0 {
            anyhow::bail!("empty region");
        }
        if start >= PAGE_SIZE32 {
            anyhow::bail!("start offset too large");
        }
        let cap = gpas.len() as u64 * PAGE_SIZE64;
        if cap < len || cap - len < start as u64 {
            anyhow::bail!("not enough pages");
        }
        Ok(Self {
            gpns: gpas,
            start: start as usize,
            len: len as usize,
        })
    }

    pub fn double(&mut self) {
        assert!(self.is_aligned_to(PAGE_SIZE64 as usize));
        self.gpns.extend_from_within(..);
        self.len *= 2;
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_aligned_to(&self, align: usize) -> bool {
        assert!(align <= PAGE_SIZE64 as usize);
        (self.start | self.len).is_multiple_of(align)
    }

    pub fn range(&self) -> PagedRange<'_> {
        PagedRange::new(self.start, self.len, &self.gpns).unwrap()
    }
}
