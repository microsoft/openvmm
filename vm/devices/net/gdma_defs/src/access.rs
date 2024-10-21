// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tools to help access queue entries.

use crate::Wqe;
use guestmem::AccessError;
use guestmem::GuestMemory;
use guestmem::MemoryRead;
use guestmem::MemoryWrite;
use std::num::Wrapping;

#[derive(Clone)]
pub struct WqeAccess<'a> {
    wqe: &'a Wqe,
    gm: &'a GuestMemory,
    sgi: usize,
    offset: usize,
    remaining_len: usize,
}

impl Wqe {
    pub fn access<'a>(&'a self, gm: &'a GuestMemory) -> WqeAccess<'a> {
        let remaining_len = if self.header.params.sgl_direct() {
            self.header.sgl_direct_len()
        } else {
            self.sgl()
                .iter()
                .map(|sge| Wrapping(sge.size as usize))
                .sum::<Wrapping<usize>>()
                .0
        };
        WqeAccess {
            wqe: self,
            gm,
            sgi: 0,
            offset: 0,
            remaining_len,
        }
    }
}

impl WqeAccess<'_> {
    fn access(
        &mut self,
        mut len: usize,
        mut f: impl FnMut(usize, Result<u64, usize>) -> Result<(), AccessError>,
    ) -> Result<&mut Self, AccessError> {
        if self.wqe.header.params.sgl_direct() {
            let avail = self.wqe.header.sgl_direct_len() - self.offset;
            if avail < len {
                return Err(AccessError::OutOfRange(avail, len));
            }
            let offset = self.wqe.header.sgl_offset() + self.offset;
            f(len, Err(offset))?;
            self.offset += len;
            self.remaining_len -= len;
        } else {
            while len > 0 {
                let sge = self.wqe.sgl().get(self.sgi).ok_or_else(|| {
                    AccessError::OutOfRange(
                        self.sgi,
                        self.wqe.header.params.num_sgl_entries().into(),
                    )
                })?;
                let gpa = sge.address.wrapping_add(self.offset as u64);
                let this_len = (sge.size as usize - self.offset).min(len);
                f(this_len, Ok(gpa))?;
                self.offset += this_len;
                self.remaining_len -= this_len;
                len -= this_len;
                if sge.size as usize == self.offset {
                    self.offset = 0;
                    self.sgi += 1;
                }
            }
        }
        Ok(self)
    }
}

impl MemoryRead for WqeAccess<'_> {
    fn read(&mut self, data: &mut [u8]) -> Result<&mut Self, AccessError> {
        let mut offset = 0;
        self.access(data.len(), |len, r| {
            match r {
                Ok(gpa) => {
                    self.gm
                        .read_at(gpa, &mut data[offset..offset + len])
                        .map_err(AccessError::Memory)?;
                    offset += len;
                }
                Err(offset) => {
                    data.copy_from_slice(&self.wqe.data[offset..offset + data.len()]);
                }
            }
            Ok(())
        })
    }

    fn skip(&mut self, len: usize) -> Result<&mut Self, AccessError> {
        self.access(len, |_, _| Ok(()))
    }

    fn len(&self) -> usize {
        self.remaining_len
    }
}

impl MemoryWrite for WqeAccess<'_> {
    fn write(&mut self, data: &[u8]) -> Result<(), AccessError> {
        let mut offset = 0;
        self.access(data.len(), |len, r| {
            match r {
                Ok(gpa) => {
                    self.gm
                        .write_at(gpa, &data[offset..offset + len])
                        .map_err(AccessError::Memory)?;
                    offset += len;
                }
                Err(offset) => {
                    // Can't receive into SGL direct.
                    return Err(AccessError::OutOfRange(offset, 0));
                }
            }
            Ok(())
        })?;
        Ok(())
    }

    fn zero(&mut self, _len: usize) -> Result<(), AccessError> {
        unimplemented!()
    }

    fn len(&self) -> usize {
        self.remaining_len
    }
}
