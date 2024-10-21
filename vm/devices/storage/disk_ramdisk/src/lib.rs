// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! RAM-backed disk backend implementation.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod resolver;

use anyhow::Context;
use disk_backend::zerodisk::InvalidGeometry;
use disk_backend::zerodisk::ZeroDisk;
use disk_backend::AsyncDisk;
use disk_backend::DiskError;
use disk_backend::SimpleDisk;
use disk_backend::Unmap;
use disk_backend::ASYNC_DISK_STACK_SIZE;
use guestmem::MemoryRead;
use guestmem::MemoryWrite;
use inspect::Inspect;
use parking_lot::RwLock;
use scsi_buffers::RequestBuffers;
use stackfuture::StackFuture;
use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Debug;
use std::future::ready;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use thiserror::Error;

/// A disk backed entirely by RAM.
pub struct RamDisk {
    data: RwLock<BTreeMap<u64, Sector>>,
    sector_count: AtomicU64,
    read_only: bool,
    lower_is_zero: bool,
    lower: Arc<dyn SimpleDisk>,
    resize_event: event_listener::Event,
}

impl Inspect for RamDisk {
    fn inspect(&self, req: inspect::Request<'_>) {
        req.respond()
            .field_with("committed_size", || {
                self.data.read().len() * size_of::<Sector>()
            })
            .field("lower_type", self.lower.disk_type())
            .field("lower", &self.lower)
            .field_mut_with("sector_count", |new_count| {
                if let Some(new_count) = new_count {
                    self.resize(new_count.parse().context("invalid sector count")?)?;
                }
                anyhow::Ok(self.sector_count())
            });
    }
}

impl Debug for RamDisk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RamDisk")
            .field("sector_count", &self.sector_count)
            .field("read_only", &self.read_only)
            .finish()
    }
}

/// An error creating a RAM disk.
#[derive(Error, Debug)]
pub enum Error {
    /// Invalid disk geometry.
    #[error(transparent)]
    InvalidGeometry(#[from] InvalidGeometry),
    /// Unsupported sector size.
    #[error("unsupported sector size {0}")]
    UnsupportedSectorSize(u32),
}

struct Sector([u8; 512]);

const SECTOR_SIZE: u32 = 512;

impl RamDisk {
    /// Makes a new RAM disk of `size` bytes.
    pub fn new(len: u64, read_only: bool) -> Result<Self, Error> {
        Self::new_inner(Arc::new(ZeroDisk::new(SECTOR_SIZE, len)?), read_only, true)
    }

    /// Makes a new RAM diff disk on top of `lower`.
    ///
    /// Writes will be collected in RAM, but reads will go to the lower disk for
    /// sectors that have not yet been overwritten.
    pub fn diff(lower: Arc<dyn SimpleDisk>, read_only: bool) -> Result<Self, Error> {
        Self::new_inner(lower, read_only, false)
    }

    fn new_inner(
        lower: Arc<dyn SimpleDisk>,
        read_only: bool,
        lower_is_zero: bool,
    ) -> Result<Self, Error> {
        let sector_size = lower.sector_size();
        if sector_size != SECTOR_SIZE {
            return Err(Error::UnsupportedSectorSize(sector_size));
        }
        let sector_count = lower.sector_count();
        Ok(Self {
            data: RwLock::new(BTreeMap::new()),
            sector_count: sector_count.into(),
            read_only,
            lower_is_zero,
            lower,
            resize_event: Default::default(),
        })
    }

    fn resize(&self, new_sector_count: u64) -> anyhow::Result<()> {
        if new_sector_count == 0 {
            anyhow::bail!("invalid sector count");
        }
        // Remove any truncated data and update the sector count under the lock.
        let _removed = {
            let mut data = self.data.write();
            self.sector_count.store(new_sector_count, Ordering::Relaxed);
            data.split_off(&new_sector_count)
        };
        self.resize_event.notify(usize::MAX);
        Ok(())
    }
}

impl SimpleDisk for RamDisk {
    fn disk_type(&self) -> &str {
        "ram"
    }

    fn sector_count(&self) -> u64 {
        self.sector_count.load(Ordering::Relaxed)
    }

    fn sector_size(&self) -> u32 {
        SECTOR_SIZE
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        self.lower.disk_id()
    }

    fn physical_sector_size(&self) -> u32 {
        self.lower.physical_sector_size()
    }

    fn is_fua_respected(&self) -> bool {
        true
    }

    fn unmap(&self) -> Option<&dyn Unmap> {
        self.lower_is_zero.then_some(self)
    }
}

impl AsyncDisk for RamDisk {
    fn read_vectored<'a>(
        &'a self,
        buffers: &'a RequestBuffers<'a>,
        sector: u64,
    ) -> StackFuture<'a, Result<(), DiskError>, { ASYNC_DISK_STACK_SIZE }> {
        StackFuture::from(async move {
            let count = (buffers.len() / SECTOR_SIZE as usize) as u64;
            tracing::trace!(sector, count, "read");
            // Always read the full lower and then overlay the changes.
            // Optimizations are possible, but some heuristics are necessary to
            // avoid lots of small reads when the disk is "Swiss cheesed".
            //
            // Box the future because otherwise it won't fit in this StackFuture.
            Box::pin(self.lower.read_vectored(buffers, sector)).await?;
            for (&s, buf) in self.data.read().range(sector..sector + count) {
                let offset = (s - sector) as usize * SECTOR_SIZE as usize;
                buffers
                    .subrange(offset, SECTOR_SIZE as usize)
                    .writer()
                    .write(&buf.0)?;
            }
            Ok(())
        })
    }

    fn write_vectored<'a>(
        &'a self,
        buffers: &'a RequestBuffers<'a>,
        sector: u64,
        _fua: bool,
    ) -> StackFuture<'a, Result<(), DiskError>, { ASYNC_DISK_STACK_SIZE }> {
        StackFuture::from(async move {
            assert!(!self.read_only);

            let count = buffers.len() / SECTOR_SIZE as usize;
            tracing::trace!(sector, count, "write");

            let mut data = self.data.write();
            for i in 0..count {
                let cur = i + sector as usize;
                let buf = buffers.subrange(i * SECTOR_SIZE as usize, SECTOR_SIZE as usize);
                let mut reader = buf.reader();
                match data.entry(cur as u64) {
                    Entry::Vacant(entry) => {
                        entry.insert(Sector(reader.read_plain()?));
                    }
                    Entry::Occupied(mut entry) => {
                        reader.read(&mut entry.get_mut().0)?;
                    }
                }
            }

            Ok(())
        })
    }

    fn sync_cache(&self) -> StackFuture<'_, Result<(), DiskError>, { ASYNC_DISK_STACK_SIZE }> {
        tracing::trace!("sync_cache");
        StackFuture::from(ready(Ok(())))
    }

    fn wait_resize<'a>(
        &'a self,
        sector_count: u64,
    ) -> Pin<Box<dyn 'a + Send + Future<Output = u64>>> {
        Box::pin(async move {
            loop {
                let listen = self.resize_event.listen();
                let current = self.sector_count();
                if current != sector_count {
                    break current;
                }
                listen.await;
            }
        })
    }
}

impl Unmap for RamDisk {
    fn unmap(
        &self,
        sector_offset: u64,
        sector_count: u64,
        _block_level_only: bool,
    ) -> StackFuture<'_, Result<(), DiskError>, { ASYNC_DISK_STACK_SIZE }> {
        assert!(self.lower_is_zero);
        StackFuture::from(async move {
            tracing::trace!(sector_offset, sector_count, "unmap");
            let mut data = self.data.write();
            // Sadly, there appears to be no way to remove a range of entries
            // from a btree map.
            let mut next_sector = sector_offset;
            let end = sector_offset + sector_count;
            while next_sector < end {
                let Some((&sector, _)) = data.range_mut(next_sector..).next() else {
                    break;
                };
                if sector >= end {
                    break;
                }
                data.remove(&sector);
                next_sector = sector + 1;
            }
            Ok(())
        })
    }

    fn optimal_unmap_sectors(&self) -> u32 {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::RamDisk;
    use super::SECTOR_SIZE;
    use crate::SimpleDisk;
    use guestmem::GuestMemory;
    use pal_async::async_test;
    use scsi_buffers::OwnedRequestBuffers;
    use std::sync::Arc;
    use zerocopy::AsBytes;

    const SECTOR_U64: u64 = SECTOR_SIZE as u64;
    const SECTOR_USIZE: usize = SECTOR_SIZE as usize;

    fn check(mem: &GuestMemory, sector: u64, start: usize, count: usize, high: u8) {
        let mut buf = vec![0u32; count * SECTOR_USIZE / 4];
        mem.read_at(start as u64 * SECTOR_U64, buf.as_bytes_mut())
            .unwrap();
        for (i, &b) in buf.iter().enumerate() {
            let offset = sector * SECTOR_U64 + i as u64 * 4;
            let expected = (offset as u32 / 4) | ((high as u32) << 24);
            assert!(
                b == expected,
                "at sector {}, word {}, got {:#x}, expected {:#x}",
                offset / SECTOR_U64,
                (offset % SECTOR_U64) / 4,
                b,
                expected
            );
        }
    }

    async fn read(mem: &GuestMemory, disk: &mut impl SimpleDisk, sector: u64, count: usize) {
        disk.read_vectored(
            &OwnedRequestBuffers::linear(0, count * SECTOR_USIZE, true).buffer(mem),
            sector,
        )
        .await
        .unwrap();
    }

    async fn write(
        mem: &GuestMemory,
        disk: &mut impl SimpleDisk,
        sector: u64,
        count: usize,
        high: u8,
    ) {
        let buf: Vec<_> = (sector * SECTOR_U64 / 4..(sector + count as u64) * SECTOR_U64 / 4)
            .map(|x| x as u32 | ((high as u32) << 24))
            .collect();
        let len = SECTOR_USIZE * count;
        mem.write_at(0, &buf.as_bytes()[..len]).unwrap();

        disk.write_vectored(
            &OwnedRequestBuffers::linear(0, len, false).buffer(mem),
            sector,
            false,
        )
        .await
        .unwrap();
    }

    #[async_test]
    async fn diff() {
        const SIZE: usize = 1024 * 1024;

        let guest_mem = GuestMemory::allocate(SIZE);

        let mut lower = RamDisk::new(SIZE as u64, false).unwrap();
        write(&guest_mem, &mut lower, 0, SIZE / SECTOR_USIZE, 0).await;
        let mut upper = RamDisk::diff(Arc::new(lower), false).unwrap();
        read(&guest_mem, &mut upper, 10, 2).await;
        check(&guest_mem, 10, 0, 2, 0);
        write(&guest_mem, &mut upper, 10, 2, 1).await;
        write(&guest_mem, &mut upper, 11, 1, 2).await;
        read(&guest_mem, &mut upper, 9, 5).await;
        check(&guest_mem, 9, 0, 1, 0);
        check(&guest_mem, 10, 1, 1, 1);
        check(&guest_mem, 11, 2, 1, 2);
        check(&guest_mem, 12, 3, 1, 0);
    }
}
