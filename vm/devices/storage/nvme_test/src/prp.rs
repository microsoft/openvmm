// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Types for parsing NVMe PRP (Physical Region Page) entries and lists.

use crate::PAGE_MASK;
use crate::PAGE_SHIFT;
use crate::PAGE_SIZE;
#[cfg(test)]
use crate::PAGE_SIZE64;
use crate::error::NvmeError;
use crate::spec;
use guestmem::GuestMemory;
use guestmem::ranges::PagedRange;
use zerocopy::IntoBytes;

const PRP_PER_PAGE: usize = PAGE_SIZE / 8;

enum PrpPfns {
    Short([u64; 2]),
    Long(Vec<u64>),
}

pub struct PrpRange {
    offset: usize,
    len: usize,
    pfns: PrpPfns,
}

impl PrpRange {
    /// Parses a PRP range for memory of `len` bytes, from the two PRP values
    /// in `prp`.
    pub fn parse(mem: &GuestMemory, len: usize, prp: [u64; 2]) -> Result<Self, NvmeError> {
        let offset = prp[0] as usize & (PAGE_SIZE - 1);
        let pfns = if len + offset <= PAGE_SIZE * 2 {
            PrpPfns::Short(prp.map(|x| x >> PAGE_SHIFT))
        } else {
            let count = (offset + len).div_ceil(PAGE_SIZE);
            let mut v = vec![0; count];
            v[0] = prp[0];
            let mut pfns = &mut v[1..];
            let mut next_prp_list = prp[1];
            loop {
                let n = pfns.len().min(PRP_PER_PAGE);
                mem.read_at(next_prp_list, pfns[..n].as_mut_bytes())
                    .map_err(|err| NvmeError::new(spec::Status::DATA_TRANSFER_ERROR, err))?;
                if n == pfns.len() {
                    break;
                }
                next_prp_list = pfns[n - 1] & PAGE_MASK;
                pfns = &mut pfns[n - 1..];
            }
            for gpa in &mut v {
                *gpa >>= PAGE_SHIFT;
            }
            PrpPfns::Long(v)
        };
        Ok(Self { offset, len, pfns })
    }

    #[cfg(test)]
    pub fn new(mut gpas: Vec<u64>, offset: usize, len: u64) -> Result<Self, &'static str> {
        for gpa in &mut gpas {
            if *gpa % PAGE_SIZE64 != 0 {
                return Err("page address is not 4KB aligned");
            }
            *gpa /= PAGE_SIZE64;
        }
        if len == 0 {
            return Err("empty region");
        }
        if offset >= PAGE_SIZE {
            return Err("start offset too large");
        }

        let pfns = {
            match gpas.len() {
                2 => PrpPfns::Short([gpas[0], gpas[1]]),
                1 => PrpPfns::Short([gpas[0], 0]),
                _ => PrpPfns::Long(gpas),
            }
        };

        Ok(Self {
            len: len as usize,
            offset,
            pfns,
        })
    }

    /// Returns the range as a [`PagedRange`].
    pub fn range(&self) -> PagedRange<'_> {
        PagedRange::new(
            self.offset,
            self.len,
            match &self.pfns {
                PrpPfns::Short(pfns) => pfns,
                PrpPfns::Long(pfns) => pfns,
            },
        )
        .unwrap()
    }

    /// Reads from the range.
    pub fn read(&self, mem: &GuestMemory, buf: &mut [u8]) -> Result<(), NvmeError> {
        mem.read_range(&self.range().subrange(0, buf.len()), buf)
            .map_err(|err| NvmeError::new(spec::Status::DATA_TRANSFER_ERROR, err))?;
        Ok(())
    }

    /// Writes to the range.
    pub fn write(&self, mem: &GuestMemory, buf: &[u8]) -> Result<(), NvmeError> {
        mem.write_range(&self.range().subrange(0, buf.len()), buf)
            .map_err(|err| NvmeError::new(spec::Status::DATA_TRANSFER_ERROR, err))?;
        Ok(())
    }

    /// Writes zeroes to the range.
    pub fn zero(&self, mem: &GuestMemory, len: usize) -> Result<(), NvmeError> {
        mem.zero_range(&self.range().subrange(0, len))
            .map_err(|err| NvmeError::new(spec::Status::DATA_TRANSFER_ERROR, err))?;
        Ok(())
    }
}
