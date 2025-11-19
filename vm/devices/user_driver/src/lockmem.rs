// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(target_os = "linux")]

use crate::memory::MappedDmaTarget;
use anyhow::Context;
use inspect::Inspect;
use std::ffi::c_void;
use std::fs::File;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use zerocopy::IntoBytes;

const PAGE_SIZE: usize = 4096;

pub struct LockedMemory {
    mapping: Mapping,
    pfns: Vec<u64>,
}

// SAFETY: The result of an mmap is safe to share amongst threads.
unsafe impl Send for Mapping {}
// SAFETY: The result of an mmap is safe to share amongst threads.
unsafe impl Sync for Mapping {}

struct Mapping {
    addr: *mut c_void,
    len: usize,
}

impl Mapping {
    fn new(len: usize) -> std::io::Result<Self> {
        // SAFETY: No file descriptor or address is being passed.
        // The result is being validated.
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_LOCKED,
                -1,
                0,
            )
        };
        if addr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }

        Ok(Self { addr, len })
    }

    fn lock(&self) -> std::io::Result<()> {
        // SAFETY: self contains a valid mmap result.
        if unsafe { libc::mlock(self.addr, self.len) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    fn pages(&self) -> anyhow::Result<Vec<u64>> {
        let mut pagemap = File::open("/proc/self/pagemap").context("failed to open pagemap")?;
        pagemap
            .seek(SeekFrom::Start((8 * self.addr as usize / PAGE_SIZE) as u64))
            .context("failed to seek")?;
        let n = self.len / PAGE_SIZE;
        let mut pfns = vec![0u64; n];
        pagemap
            .read(pfns.as_mut_bytes())
            .context("failed to read from pagemap")?;
        for pfn in &mut pfns {
            if *pfn & (1 << 63) == 0 {
                anyhow::bail!("page not present in RAM");
            }
            *pfn &= 0x3f_ffff_ffff_ffff;
        }
        Ok(pfns)
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: self contains a valid mmap result.
        if unsafe { libc::munmap(self.addr, self.len) } < 0 {
            panic!("{:?}", std::io::Error::last_os_error());
        }
    }
}

impl LockedMemory {
    pub fn new(len: usize) -> anyhow::Result<Self> {
        if !len.is_multiple_of(PAGE_SIZE) {
            anyhow::bail!("not a page-size multiple");
        }
        let mapping = Mapping::new(len).context("failed to create mapping")?;
        mapping.lock().context("failed to lock mapping")?;
        let pages = mapping.pages()?;
        Ok(Self {
            mapping,
            pfns: pages,
        })
    }
}

// SAFETY: The stored mapping is valid for the lifetime of the LockedMemory.
// It is only unmapped on drop.
unsafe impl MappedDmaTarget for LockedMemory {
    fn base(&self) -> *const u8 {
        self.mapping.addr.cast()
    }

    fn len(&self) -> usize {
        self.mapping.len
    }

    fn pfns(&self) -> &[u64] {
        &self.pfns
    }

    fn pfn_bias(&self) -> u64 {
        0
    }
}

/// A DMA client spawner that allocates memory at arbitrary locations in VTL2's
/// address space. Use this spawner when you don't care about the PFNs being
/// contiguous or able to survive an OpenHCL servicing event.
#[derive(Clone, Inspect)]
pub struct LockedMemorySpawner;

impl crate::DmaClient for LockedMemorySpawner {
    fn allocate_dma_buffer(&self, len: usize) -> anyhow::Result<crate::memory::MemoryBlock> {
        Ok(crate::memory::MemoryBlock::new(LockedMemory::new(len)?))
    }

    fn attach_pending_buffers(&self) -> anyhow::Result<Vec<crate::memory::MemoryBlock>> {
        anyhow::bail!("restore not supported for lockmem")
    }
}
