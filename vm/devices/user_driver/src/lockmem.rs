// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(target_os = "linux")]

use crate::memory::MappedDmaTarget;
use anyhow::Context;
use fs_err::os::unix::fs::OpenOptionsExt;
use std::ffi::c_void;
use std::fs::File;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::os::unix::prelude::*;
use zerocopy::AsBytes;

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
    fn new(len: usize) -> anyhow::Result<Self> {
        // Create a ramfs file to back the mapping. This is necessary to ensure
        // the memory is not moved (which is possible for ordinary
        // tmpfs/anonymous allocations, even when mlocked).
        //
        // FUTURE: investigate other mechanisms for this, since some
        // enterprising kernel developer may change ramfs to use movable memory
        // one day. Ideally, we'd use a proper IOMMU, but that's still not
        // available in the paravisor environment.
        let file = unsafe {
            let fd = libc::syscall(libc::SYS_memfd_secret, libc::O_CLOEXEC as usize);
            if fd < 0 {
                return Err(std::io::Error::last_os_error())
                    .context("failed to create memfd_secret file");
            }
            File::from_raw_fd(fd as i32)
        };

        file.set_len(len as u64)
            .context("failed to set ramfs file length")?;

        // SAFETY: No file descriptor or address is being passed.
        // The result is being validated.
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if addr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error()).context("failed to map memory");
        }
        let this = Self { addr, len };

        // Force populate the PTEs by zeroing the allocation. MAP_POPULATE does
        // not work for memfd_secret mappings.
        //
        // SAFETY: The memory is valid for write.
        unsafe { addr.cast::<u8>().write_bytes(0, len) };

        Ok(this)
    }

    fn pages(&self) -> anyhow::Result<Vec<u64>> {
        let mut pagemap = File::open("/proc/self/pagemap").context("failed to open pagemap")?;
        pagemap
            .seek(SeekFrom::Start(
                (8 * (self.addr as usize / PAGE_SIZE)) as u64,
            ))
            .context("failed to seek")?;
        let n = self.len / PAGE_SIZE;
        let mut pfns = vec![0u64; n];
        pagemap
            .read(pfns.as_bytes_mut())
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
        if len % PAGE_SIZE != 0 {
            anyhow::bail!("not a page-size multiple");
        }
        let mapping = Mapping::new(len).context("failed to create mapping")?;
        let pages = mapping
            .pages()
            .context("failed to get pfns for DMA buffer")?;
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
}

#[derive(Clone)]
pub struct LockedMemorySpawner;

#[cfg(feature = "vfio")]
impl crate::vfio::VfioDmaBuffer for LockedMemorySpawner {
    fn create_dma_buffer(&self, len: usize) -> anyhow::Result<crate::memory::MemoryBlock> {
        Ok(crate::memory::MemoryBlock::new(LockedMemory::new(len)?))
    }

    /// Restore mapped DMA memory at the same physical location after servicing.
    fn restore_dma_buffer(
        &self,
        _len: usize,
        _base_pfn: u64,
    ) -> anyhow::Result<crate::memory::MemoryBlock> {
        anyhow::bail!("restore not supported for lockmem")
    }
}
