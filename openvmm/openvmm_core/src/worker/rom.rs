// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ROM implementation based on a shared memory object.
//!
//! FUTURE: Consider implementing the Hyper-V pseudo ROMs by putting them
//! somewhere in guest memory that will be untouched during the early boot
//! process. This will save having to allocate and migrate additional objects.

use guestmem::MapRom;
use guestmem::MappableGuestMemory;
use guestmem::MemoryMapper;
use guestmem::UnmapRom;
use openvmm_pcat_locator::RomFileLocation;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;

pub struct Rom {
    mapper: Box<dyn MemoryMapper>,
    name: String,
    len: u64,
    backing: sparse_mmap::Mappable,
}

pub struct RomBuilder {
    name: String,
    mapper: Box<dyn MemoryMapper>,
}

impl RomBuilder {
    pub fn new(name: String, mapper: Box<dyn MemoryMapper>) -> Self {
        Self { name, mapper }
    }

    /// Constructs a ROM from the specified bytes in the specified file.
    pub fn build_from_file_location(self, details: &RomFileLocation) -> std::io::Result<Rom> {
        let mut file = &details.file;
        file.seek(SeekFrom::Start(details.start))?;
        let mut buf = vec![0; details.len];
        file.read_exact(&mut buf)?;
        self.build_from_slice(&buf)
    }

    /// Constructs a ROM from the specified data.
    fn build_from_slice(self, data: &[u8]) -> std::io::Result<Rom> {
        let backing = sparse_mmap::alloc_shared_memory(data.len())?;
        let mapping = sparse_mmap::SparseMapping::new(data.len())?;
        mapping.map_file(0, data.len(), &backing, 0, true)?;
        mapping.write_at(0, data).unwrap();
        Ok(Rom {
            name: self.name,
            mapper: self.mapper,
            len: data.len() as u64,
            backing,
        })
    }
}

impl MapRom for Rom {
    fn map_rom(&self, gpa: u64, offset: u64, len: u64) -> std::io::Result<Box<dyn UnmapRom>> {
        assert!(offset + len <= self.len);
        let (mut memory, region) = self.mapper.new_region(len as usize, self.name.clone())?;
        region.map(0, &self.backing, offset, len as usize, false)?;
        memory.map_to_guest(gpa, false)?;
        Ok(Box::new(MappedRom(memory)))
    }

    fn len(&self) -> u64 {
        self.len
    }
}

struct MappedRom(Box<dyn MappableGuestMemory>);

impl UnmapRom for MappedRom {
    fn unmap_rom(mut self) {
        self.0.unmap_from_guest();
    }
}
